//! S5 (host-class shims): `java.util.Random` (bit-exact JVM LCG),
//! `java.util.Date`, `Thread`/`Thread/currentThread`, and a narrow `proxy
//! [ThreadLocal] [] (initialValue [] ...)` -- FOUR unrelated host classes
//! sharing exactly ONE new `Value` variant (`Value::HostInst`, see that
//! variant's own doc comment for why one cell rather than four). This
//! module is the ONE place that dispatches on `HostKind` -- construction
//! (`construct`, called from `eval::types_forms::eval_new` for a builtin
//! `ClassVal`) and method calls (`call_method`, called from
//! `eval::types_forms::eval_dot_form` when the `.method` target is a
//! `Value::HostInst`) both live here so nothing else in the crate needs to
//! know these four classes exist.
//!
//! S6 adds a FIFTH, unrelated arm to `construct` below: ten boxed-
//! numeric/`Boolean`/`Character`/`BigDecimal`/`BigInteger` constructors
//! (`(Long. x)`, `(Float. x)`, ...). Unlike the four HostKind classes
//! above, these mint no new `Value` shape at all -- see the doc comment
//! on the `boxed_ctor` arm group (right after `construct`'s `match`) for
//! why, and for the "compat veneer, not JVM emulation" framing that
//! governs how far this module goes with them.
//!
//! ## `java.util.Random`
//!
//! Ground truth: the `java.util.Random` javadoc's exact LCG (`seed =
//! (seed * 0x5DEECE66D + 0xB) & ((1<<48)-1)`, `next(bits) = seed >>> (48 -
//! bits)`). This is deliberately bit-for-bit, not merely
//! statistically-plausible: `data.generators`/`test.check`'s
//! `java-util-random` fallback (and any corpus form seeding a `Random`
//! with a literal) depends on reproducing the EXACT sequence real
//! `java.util.Random` produces for a given seed -- see
//! `tests/conformance/corpus/hostclasses.corpus`'s measured transcript and
//! this module's own `#[cfg(test)]` block, both checked against the
//! `.oracle` JVM.
//!
//! ## `java.util.Date`
//!
//! Just an epoch-millis cell (`.getTime`, and the two constructors). No
//! `.toString`/`.compareTo`/etc -- real `Date.toString()` is
//! locale/timezone-dependent (unfit for a corpus golden anyway) and
//! nothing in this task's scope needs the rest of `Date`'s surface.
//!
//! ## `Thread`
//!
//! `Thread/currentThread` returns a stand-in single "main" thread
//! (`.getName` -> `"main"`, `.getId` -> `1`, `.getStackTrace` -> an empty
//! array) -- unchanged.
//!
//! C3c ADDS a real `(Thread. f)` constructor (delays.clj's
//! `calls-once-in-parallel`/`saves-exceptions-in-parallel`, vars.clj's
//! `test-with-redefs`/`test-with-redefs-fn`, all of which spawn real
//! `Thread`s and need genuine concurrent progress, not a single-threaded
//! stub): mova is NOT single-threaded under the hood -- `future*`
//! (`builtins::conc`) already spawns real OS threads via
//! `std::thread::Builder`, sharing the SAME `globals` `Env` `Arc` across
//! them, which is exactly what makes a root-binding mutation from one
//! thread (e.g. `with-redefs`'s var-root swap) visible to another thread
//! reading it. `(Thread. f)`'s `.start` reuses that exact mechanism
//! (same stack size, same dynamic-binding conveyance, same detached-
//! thread-publishes-into-a-cell shape) rather than inventing a second
//! one; `.join` reuses `future_deref` on that same cell. See
//! `HostState::RealThread`'s doc for the cell shape and
//! `call_thread_method`'s `"start"`/`"join"` arms for the mechanics.
//! `java.util.concurrent.CyclicBarrier` (delays.clj/vars.clj's own
//! rendezvous point across N+1 real threads) rides `std::sync::Barrier`
//! directly -- see `HostState::CyclicBarrier`'s doc.
//!
//! ## `proxy [ThreadLocal]`
//!
//! Exactly the one shape `clojure.test.check.random/next-rng` needed
//! (`test.check` 1.1.3's `random.clj:178`) -- **needED**: SPEC-W6a made
//! that namespace a Rust-native veneer and its seedless `make-random`
//! now uses a real Rust `thread_local!`
//! (`crate::builtins::tcrandom::THREAD_RNG`), so nothing in the shipped
//! stdlib reaches this arm any longer. It stays because it is a
//! correct, tested, self-contained veneer for a shape user code can
//! still write, not because anything here depends on it; the shape it
//! supports is unchanged:
//! `(proxy [ThreadLocal] [] (initialValue [] ...))` -- empty
//! superclass-constructor args, ZERO or ONE method override
//! (`initialValue`, 0-arity). mova is single-threaded here, so "thread
//! local" degenerates to a lazy box: `.get` memoizes the (0-arity)
//! `initialValue` override's result (or `nil`, matching real
//! `ThreadLocal.get()`'s default when no override exists) on first call,
//! `.set` overwrites it, `.remove` clears it back to "not yet computed" so
//! the NEXT `.get` re-invokes `initialValue` (measured against the real
//! JVM: `ThreadLocal.remove()` followed by `.get()` re-runs
//! `initialValue`, it does not return the value from before `.set`/the
//! first `.get`). `eval::special_forms`'s `proxy` special-form handler
//! parses this ONE shape and calls `mk_threadlocal` below; any other
//! `proxy` shape (a different superclass, non-empty ctor args, more than
//! one method override, an override of anything but `initialValue`) is a
//! clear, documented "not supported" error -- deliberately NOT a general
//! `proxy` implementation (see that handler's own doc for why).

use std::sync::atomic::{AtomicI64, Ordering};
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};

use num_bigint::BigInt;

use crate::bignum::{BigDecVal, BigIntVal};
use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::reader::Span;
use crate::value::{ArrayKind, ArrayVal, PMap, PVec, Str, Value};

/// Which of the [now six] unrelated host classes a [`HostInstVal`] cell
/// is. `StringBuilder`/`StringBuffer` (S6/predicates.clj batch) are
/// CONSTRUCTION-ONLY -- see `construct`'s doc on those two arms for why
/// no method surface (`.append`/`.toString`/...) is implemented: nothing
/// in the vendored suite calls one (measured, `grep -rn StringBuil[dt]er
/// tests/clojure-suite/vendor*`), and `predicates.clj`'s one use
/// (`(not (string? (StringBuilder. "abc")))`) only needs the instance to
/// exist and not be a `Value::Str` -- exactly `java.util.Date`'s existing
/// "no `.toString`/`.compareTo`/etc, nothing in scope needs the rest of
/// the surface" precedent (this module's own doc, above).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    Random,
    Date,
    Thread,
    ThreadLocal,
    StringBuilder,
    StringBuffer,
    /// C7 (vecveneer): `java.util.ArrayList` -- CONSTRUCTION-ONLY plus
    /// cross-type equality with a vector, same narrow scope as
    /// `StringBuilder`/`StringBuffer` above (no `.add`/`.get`/etc method
    /// surface -- `test-vector-eqv-to-non-counted-types`'s ONE call site,
    /// `(= [0 1 2] (new java.util.ArrayList [0 1 2]))`, only needs the
    /// instance to exist and compare `=` to a vector). See `HostState::
    /// ArrayList`'s doc for the stored payload and `value.rs`'s
    /// `values_equal`/`PartialEq` arms for the equality wiring.
    ArrayList,
    /// C10 (data_structures.clj): `java.util.HashMap` -- UNLIKE
    /// `ArrayList`, this one is genuinely mutable (`.put`, the ONE method
    /// the vendored suite calls: `test-map-entry?`'s `(doto
    /// (java.util.HashMap.) (.put "x" 1))`) because a bare `(java.util.
    /// HashMap.)` with nothing put into it yet is useless for that test's
    /// purpose. Beyond `.put`, still narrow: no `.remove`/`.size`/
    /// `.keySet`/etc -- `contains?`/`get`/`count`/`seq`/`first` all read
    /// the same `HostState::HashMap` cell directly (see `eval::mod.rs`'s
    /// `seq_items` and `builtins/collections.rs`'s `get`/`contains?`/
    /// `count` arms), so those don't need a `.method` surface at all, only
    /// `.put` does.
    HashMap,
    /// C10: `java.util.HashSet` -- construction-only, same rationale as
    /// `ArrayList` (only `test-contains?`'s `(java.util.HashSet. #{..})`
    /// construction-with-content shape is exercised; no `.add`/`.remove`
    /// call site exists in the vendored suite).
    HashSet,
    /// C10: bare `(new Object)` -- `data_structures.clj`'s `test-
    /// equality` needs exactly one thing from it: a fresh, non-`nil`
    /// value that's never `=` to anything else (measured: real Java's
    /// default `Object.equals`/`hashCode` are identity-based, and
    /// `java.lang.Object` itself overrides neither). Zero state,
    /// construction-only, no method surface here -- `Arc` pointer
    /// identity (the generic `(HostInst, HostInst)` `PartialEq`/`Hash`
    /// arms, `value.rs`) already gives exactly that.
    Object,
    /// C10: `.iterator`'s return value -- `data_structures.clj`'s
    /// `test-seq-iter-match` (via its `seq-iter-match` helper) walks a
    /// map/`keys`/`vals`/`rest`-of-those with `.hasNext`/`.next`, cross-
    /// checking it lines up element-for-element with the SAME
    /// collection's `seq`. A mutable cursor over an already-realized
    /// remaining-sequence `Value` (see `HostState::Iterator`'s doc) --
    /// `.iterator` itself is wired generically in `eval::types_forms::
    /// eval_dot_form`'s C10 fallback (any seqable target), not per-kind
    /// here.
    Iterator,
    /// D1: `(.spliterator v)`'s return value -- a mutable cursor over a
    /// HALF-OPEN RANGE of an already-snapshotted element vector (see
    /// `HostState::Spliterator`). Three unrelated `vectors.clj` deftests
    /// walk one (`test-empty-vector-spliterator`,
    /// `test-spliterator-tryadvance-then-forEach`,
    /// `test-spliterator-trySplit`), and between them they call exactly
    /// five methods: `estimateSize`, `getExactSizeIfKnown`, `tryAdvance`,
    /// `trySplit`, `forEachRemaining`. Nothing else of
    /// `java.util.Spliterator` is implemented -- no `characteristics`, no
    /// `getComparator`, no `Spliterators` helpers.
    Spliterator,
    /// D1: `(.stream v)`/`(.parallelStream v)`'s return value. COUNTING
    /// ONLY: `test-vector-parallel-stream` is the whole of the vendored
    /// stream surface, and it does exactly one thing with a stream --
    /// `(.collect s (Collectors/counting))`. No `map`/`filter`/`reduce`/
    /// terminal-op zoo, and `parallelStream` is the same sequential
    /// cursor (parallelism is a hint on the real JVM too, and nothing
    /// observable here distinguishes the two).
    Stream,
    /// D1: `(Collectors/counting)`'s return value -- a marker, not a
    /// collector implementation. `counting` is the only `Collectors`
    /// factory the vendored suite calls, so this kind has exactly one
    /// inhabitant and `.collect` accepts nothing else; a generic
    /// `Collector` (supplier/accumulator/combiner/finisher) would be
    /// unmeasured surface.
    Collector,
    /// C3c (delays.clj's `calls-once-in-parallel`/`saves-exceptions-in-
    /// parallel`, vars.clj's `test-with-redefs`/`test-with-redefs-fn`):
    /// `java.util.concurrent.CyclicBarrier` -- see `HostState::
    /// CyclicBarrier`'s doc for the payload and `call_barrier_method`'s
    /// doc for why a real OS-thread rendezvous, not a stub, is both
    /// possible and necessary here.
    CyclicBarrier,
    /// W4-veneer (sequences.clj's `test-iteration` file-IO row):
    /// `(java.io.File. "readme.txt")` -- construction-only, no state of
    /// its own beyond the path (see `HostState::JavaFile`). `.toPath`
    /// returns the SAME `HostInst` unchanged (mova conflates `File`/
    /// `Path` into one path-holding cell -- nothing in scope tells them
    /// apart: the only consumer, `Files/newBufferedReader`, just reads
    /// the path back out, and no vendored form calls `class`/`instance?`
    /// on the `.toPath` result specifically).
    JavaFile,
    /// W4-veneer: `(java.nio.file.Files/newBufferedReader (.toPath ...))`'s
    /// return value -- see `HostState::BufferedReader`'s doc for the
    /// eagerly-split-into-lines representation and why it is honest for
    /// the ONE thing this corpus calls on one (`.readLine`, plus `.close`).
    BufferedReader,
    /// lsp/io (clj-kondo stdin campaign): `java.io.StringReader` --
    /// clj-kondo's real (non-toolsreader) stdin path is exactly one call,
    /// `clj_kondo.impl.core/process-file`'s `(= "-" path)` branch:
    /// `(slurp *in*)`. Upstream `run-kondo-on-text!` binds `*in*` with
    /// `with-in-str`, whose macroexpansion is `(clojure.lang.
    /// LineNumberingPushbackReader. (java.io.StringReader. s))` -- both
    /// wrapper ctors are IDENTITY passthroughs onto this same state (see
    /// `construct`'s arms for them), so `*in*` ends up bound to one of
    /// these regardless of which class name wrapped it last. Holds the
    /// RAW original string (not pre-split lines, unlike `BufferedReader`
    /// above) so `slurp` can hand back byte-for-byte identical content --
    /// `namespace-name-mismatch` and friends are whitespace/position
    /// sensitive, so reconstructing from `str::lines()` (which drops
    /// terminator/blank-line fidelity) would be a silent corruption bug.
    /// `.readLine`/`.close` are the only methods `clojure.core/read-line`
    /// and `with-open`'s `finally` block ever call on it.
    StringReader,
    /// W4-veneer (try_catch.clj's `catch-receives-checked-exception-from-
    /// reflective-call`): `(ReflectorTryCatchFixture.)` -- a fixture
    /// object whose only method, `.failWithCause`, throws a `Cookies`
    /// (see `call_reflector_fixture_method`). Stateless, same shape as
    /// `HostKind::Object`.
    ReflectorFixture,
    /// W4C-NS (`protocols.clj`'s `exercise-literals`, ctor-arity row):
    /// `java.util.Locale` -- construction-only, same rationale as
    /// `ArrayList`/`HashSet` above (the ONE thing this corpus needs is
    /// real 1-3-arg constructor-arity ENFORCEMENT for `#java.util.
    /// Locale[...]`'s reader-literal desugaring, `(Unexpected number of
    /// constructor arguments to class java.util.Locale: got N)` -- no
    /// `.getLanguage`/`.toString`/`.equals`/etc call site exists anywhere
    /// in the vendored suite). See `HostState::Locale`'s doc for the
    /// stored payload (kept even though nothing reads it back, matching
    /// what the real constructor would have stored).
    Locale,
    /// kondo-wave: `java.util.concurrent.locks.ReentrantLock` -- a REAL
    /// reentrant mutex (Mutex<(Option<ThreadId>, usize)> + Condvar), not
    /// a no-op -- see `call_reentrant_lock_method`'s doc.
    ReentrantLock,
    /// lsp/io (clojure-lsp-on-Mova campaign, mova/PLAN.md, review round
    /// 2): a real, general input stream -- `System/in`, an opened file,
    /// or an in-memory byte/string source -- backed by a boxed
    /// `Read + Send` trait object, buffered. See `HostState::
    /// InputStream`'s doc.
    InputStream,
    /// lsp/io: the output-side counterpart -- `System/out`/`System/err`
    /// or an opened file, backed by a boxed `Write + Send` trait object,
    /// buffered, and streamed through on `.write` (not accumulated then
    /// written whole on `.close`). See `HostState::OutputStream`'s doc.
    OutputStream,
    /// lsp/host (clojure-lsp-on-Mova campaign): `(java.time.Clock/
    /// systemDefaultZone)` -- `jsonrpc4clj.server`'s `ChanServer`
    /// record has a REAL `^java.time.Clock clock` type hint, enforced
    /// by an actual `instanceof`/cast on the real JVM (measured: a
    /// `proxy [Object] []` stand-in throws `ClassCastException` there,
    /// silently, on an async thread -- the smoke test's first real bug
    /// this campaign hit). Stateless -- every instance behaves
    /// identically, `.instant` always answers the real current time.
    Clock,
    /// lsp/host: `(.instant clock)`'s return value -- see `HostState::
    /// Instant`'s doc for the epoch-millis representation.
    Instant,
    /// lsp/kondo: `java.util.StringTokenizer` -- see `HostState::
    /// StringTokenizer`'s doc for the tokenizing algorithm.
    StringTokenizer,
    /// mova campaign (clojure-lsp): `java.security.MessageDigest` --
    /// construction-only (via `MessageDigest/getInstance "MD5"|"SHA-256"`,
    /// see `statics.rs`), see `HostState::MessageDigest`'s doc for the
    /// one method (`.digest`) this supports.
    MessageDigest,
    /// lsp/kondo: `java.util.jar.JarFile`/`java.util.zip.ZipFile` -- see
    /// `HostState::JarFile`'s doc. Both Java classes map to the same
    /// underlying `zip::ZipArchive` (`ZipFile` is `JarFile`'s superclass
    /// and clj-kondo/clojure-lsp only ever touch the `ZipFile`-level
    /// surface: `.entries`/`.getEntry`/`.getInputStream`/`.close`).
    JarFile,
    /// lsp/kondo: one `java.util.jar.JarFile$JarFileEntry` /
    /// `java.util.zip.ZipEntry` -- see `HostState::JarEntry`'s doc.
    JarEntry,
    /// lsp/kondo: `(.entries jar)`'s return value -- a Java `Enumeration`
    /// over already-collected `JarEntry` values (from the cheap,
    /// non-inflating central-directory scan), so `enumeration-seq`'s
    /// generic `.hasMoreElements`/`.nextElement` dispatch works on it.
    JarEntries,
    /// e2: `java.io.RandomAccessFile` -- only what kondo's cache lock needs (`.getChannel`/`.close`).
    RandomAccessFile,
    /// e2: `java.nio.channels.FileChannel` from `.getChannel` -- `.tryLock`/`.lock` (POSIX fcntl, JVM-compatible).
    FileChannel,
    /// e2: `java.nio.channels.FileLock` -- `.release`/`.isValid`/`.close`.
    FileLock,
    /// e2: `java.net.URL` -- `.openConnection`/`.openStream`/`str`.
    Url,
    /// e2: `javax.net.ssl.HttpsURLConnection` from `.openConnection` -- see `crate::http`.
    HttpConnection,
}

impl HostKind {
    /// `Value::type_name`'s diagnostic-only name for this kind -- NOT
    /// Clojure's `class`/`type` (that's `types::builtin_class_name`'s
    /// `Value::HostInst` arm, which reads `HostKind` too but answers with
    /// the real JVM class name instead).
    pub fn diagnostic_name(self) -> &'static str {
        match self {
            HostKind::Random => "random",
            HostKind::Date => "date",
            HostKind::Thread => "thread",
            HostKind::ThreadLocal => "threadlocal",
            HostKind::StringBuilder => "stringbuilder",
            HostKind::StringBuffer => "stringbuffer",
            HostKind::ArrayList => "arraylist",
            HostKind::HashMap => "hashmap",
            HostKind::HashSet => "hashset",
            HostKind::Object => "object",
            HostKind::Iterator => "iterator",
            HostKind::Spliterator => "spliterator",
            HostKind::Stream => "stream",
            HostKind::Collector => "collector",
            HostKind::CyclicBarrier => "cyclicbarrier",
            HostKind::JavaFile => "javafile",
            HostKind::BufferedReader => "bufferedreader",
            HostKind::StringReader => "stringreader",
            HostKind::ReflectorFixture => "reflectortrycatchfixture",
            HostKind::Locale => "locale",
            HostKind::ReentrantLock => "reentrantlock",
            HostKind::InputStream => "inputstream",
            HostKind::OutputStream => "outputstream",
            HostKind::Clock => "clock",
            HostKind::Instant => "instant",
            HostKind::StringTokenizer => "stringtokenizer",
            HostKind::MessageDigest => "messagedigest",
            HostKind::JarFile => "jarfile",
            HostKind::JarEntry => "jarentry",
            HostKind::JarEntries => "jarentries",
            HostKind::RandomAccessFile => "randomaccessfile",
            HostKind::FileChannel => "filechannel",
            HostKind::FileLock => "filelock",
            HostKind::Url => "url",
            HostKind::HttpConnection => "httpconnection",
        }
    }
}

/// `Value::HostInst`'s cell: a kind tag plus whichever of the four
/// per-kind mutable payloads below `kind` says it holds. `Arc`-shared,
/// `Mutex`-guarded -- same shape as `MatcherState`/`ArrayVal` (see
/// `Value::Matcher`/`Value::Array`'s own doc comments for the identical
/// precedent).
#[derive(Debug)]
pub struct HostInstVal {
    pub kind: HostKind,
    pub state: Mutex<HostState>,
}

/// kondo-wave: the real, inner mutex+condvar pair backing
/// `HostState::ReentrantLock` -- see that variant's doc.
#[derive(Debug, Default)]
pub struct ReentrantLockState {
    owner: Option<std::thread::ThreadId>,
    count: usize,
}

#[derive(Debug)]
pub enum HostState {
    Random(RandomState),
    /// Epoch milliseconds, Java `Date`'s entire internal state.
    Date(i64),
    /// No mutable state -- `Thread/currentThread` always answers the same
    /// stand-in "main" thread facts (see this module's doc).
    Thread,
    /// C3c (delays.clj/vars.clj, `(Thread. f)` -- see this module's doc's
    /// new "`Thread` constructor" section): a REAL, constructed thread.
    /// `f` is the 0-arity fn to run; `cell` is the exact SAME `FutureCell`
    /// shape `future*` uses (`crate::value::FutureCell`/`FutureState`) --
    /// `.start` spawns an OS thread that runs `f` and publishes into it,
    /// `.join` blocks on it, reusing `builtins::conc::future_deref`
    /// directly rather than re-deriving the same wait/notify dance.
    /// `started` guards against a second `.start` (measured: real
    /// `Thread.start()` on an already-started thread throws
    /// `IllegalThreadStateException`; nothing in this task's corpus
    /// exercises that path, but leaving it unguarded would let TWO
    /// spawns race on the same cell, which `FutureState`'s single
    /// `Done`/`Failed` write is not designed to arbitrate between).
    RealThread(RealThreadState),
    ThreadLocal(ThreadLocalState),
    /// `StringBuilder`/`StringBuffer`'s initial contents -- construction-
    /// only (see `HostKind`'s doc), so the string is never actually read
    /// back by any method; it exists purely so a value that PRINTS/
    /// EQUALS/whatever downstream code does with it has real content to
    /// work with, matching what the real constructor stored.
    CharBuf(Str),
    /// C7 (vecveneer): `java.util.ArrayList`'s elements, snapshotted at
    /// construction time (real `ArrayList(Collection)` copies, it doesn't
    /// alias) -- construction-only, like `CharBuf` above, so nothing ever
    /// mutates this after `construct` builds it (no `.add`/`.set`/etc in
    /// this module's scope, see `HostKind::ArrayList`'s doc); it exists so
    /// `=`/`pr-str`/`count` have real content to work with.
    ArrayList(crate::value::PVec),
    /// C10: `java.util.HashMap`'s backing store -- a real `PMap`, mutated
    /// in place by `.put` (unlike `ArrayList`/`CharBuf`, this one IS
    /// mutable after construction; see `HostKind::HashMap`'s doc).
    HashMap(PMap),
    /// C10: `java.util.HashSet`'s elements, snapshotted at construction
    /// time -- construction-only like `ArrayList`/`CharBuf` (see
    /// `HostKind::HashSet`'s doc).
    HashSet(champ::PersistentHashSet<Value>),
    /// C10: `(new Object)` -- no state at all, see `HostKind::Object`'s
    /// doc.
    Object,
    /// C10: `.iterator`'s cursor -- the REMAINING sequence, `Value::Nil`
    /// once exhausted (matches `uncons`'s own "no more" sentinel, so
    /// `.hasNext`/`.next` reuse it directly). `.iterator` builds this
    /// from `Interp::seq_items` at construction time -- a SNAPSHOT, same
    /// v1 simplification `Value::Array`'s `seq_items` doc already
    /// documents (a concurrent mutation to the source collection after
    /// `.iterator` is called is not reflected, real Java `Iterator`s over
    /// PERSISTENT collections have this same property anyway since the
    /// collection itself can't mutate in place).
    Iterator(Value),
    /// D1: a spliterator's ENTIRE state -- an element snapshot plus the
    /// half-open `[pos, end)` range of it this cursor still owns.
    /// `trySplit` hands the first half of that range to a NEW cursor over
    /// the SAME (persistent, cheap-to-clone) snapshot and shrinks this
    /// one's `pos` to the midpoint, so a split costs no element copying
    /// and the two halves are disjoint by construction.
    Spliterator { items: crate::value::PVec, pos: usize, end: usize },
    /// D1: a stream's elements. Immutable -- counting is the only
    /// terminal op (see `HostKind::Stream`).
    Stream(crate::value::PVec),
    /// D1: `(Collectors/counting)`'s marker -- stateless, see
    /// `HostKind::Collector`.
    Collector,
    /// C3c: `java.util.concurrent.CyclicBarrier`'s ENTIRE state -- a
    /// `std::sync::Barrier` sized to the constructor's `parties` arg.
    /// `Arc`-wrapped (on top of the `HostInstVal`'s own `Arc`) so
    /// `call_barrier_method` can clone it OUT from under the `state`
    /// mutex and call `.wait()` on the clone with the lock released --
    /// see that fn's own doc for why holding the lock across `.wait()`
    /// would deadlock every other party. `std::sync::Barrier` is
    /// deliberately reusable across generations (calling `.wait()` past
    /// the release point starts a NEW round) -- exactly `CyclicBarrier`'s
    /// own "cyclic" namesake behavior, needing no extra bookkeeping here.
    CyclicBarrier(Arc<std::sync::Barrier>),
    /// W4-veneer: a `java.io.File`'s (conflated `Path`'s) ENTIRE state --
    /// just the path string it was constructed with. Construction-only,
    /// like `CharBuf`/`ArrayList` above -- `.toPath` hands the same
    /// `HostInst` straight back (see `HostKind::JavaFile`'s doc), and
    /// `Files/newBufferedReader` only ever READS this field.
    JavaFile(Str),
    /// W4-veneer: a `BufferedReader`'s ENTIRE state -- the file's content
    /// split into lines EAGERLY, at open time (real Java buffers reads
    /// incrementally; nothing in this corpus can observe the difference,
    /// since nothing here reads a file that's concurrently written or
    /// abandons a reader before draining it), plus a cursor. `.readLine`
    /// pops the front and advances the cursor, returning `Value::Nil`
    /// once exhausted (real Java: `null`, mova's `nil`) -- matching
    /// `line-seq`'s own `nil`-terminated contract exactly, so both are
    /// driven by the identical `.readLine` primitive. `closed` is tracked
    /// but not enforced (nothing in scope calls `.readLine` after
    /// `.close`); `.close` just flips it.
    BufferedReader { lines: Vec<Str>, pos: usize, closed: bool },
    /// lsp/io: a `java.io.StringReader`'s ENTIRE state -- the RAW original
    /// string plus a byte-offset cursor (always landing on a UTF-8
    /// boundary: it only ever advances past `\n`/`\r\n`, both single-byte
    /// ASCII). See `HostKind::StringReader`'s doc for why this stores raw
    /// text rather than `BufferedReader`'s pre-split lines.
    StringReader { content: Str, pos: usize, closed: bool },
    /// W4C-NS: a `java.util.Locale`'s ENTIRE state -- language/country/
    /// variant, matching real `Locale`'s three `String` fields exactly
    /// (an unsupplied trailing part is `""`, same as the real
    /// constructor). Construction-only, like `JavaFile`/`CharBuf` above
    /// -- see `HostKind::Locale`'s doc for why nothing reads this back.
    Locale { language: Str, country: Str, variant: Str },
    /// lsp/io (review round 2): a real input stream's ENTIRE state -- a
    /// buffered, boxed `Read + Send` trait object. Sources: `System/in`
    /// (real process stdin), an opened file (`mova.io/file-input-
    /// stream`), or an in-memory byte/string source (`mova.io/string-
    /// input-stream`, a `std::io::Cursor` over the string's UTF-8
    /// bytes) -- any of the three answer to the exact same `Read`
    /// trait, which is the whole point of boxing rather than a per-
    /// source enum. `BoxedReader`'s manual `Debug` (see its own doc)
    /// is the only reason this isn't `BufReader<Box<dyn Read + Send>>`
    /// directly -- `HostState` derives `Debug`, and a trait object has
    /// none.
    InputStream(BoxedReader),
    /// lsp/io: the output-side counterpart -- `System/out`/`System/err`
    /// (real process stdout/stderr) or an opened file (`mova.io/file-
    /// output-stream`), buffered (`BufWriter`), and streamed through on
    /// every `.write`/`.append` rather than accumulated in memory and
    /// written once on `.close` (that was the previous, now-removed
    /// `FileWriter` design's simplification; a real streaming writer is
    /// what the review asked for and what a large file needs).
    OutputStream(BoxedWriter),
    /// lsp/host: `(java.time.Clock/systemDefaultZone)` -- no state,
    /// see `HostKind::Clock`'s doc.
    Clock,
    /// lsp/host: `(.instant clock)`'s ENTIRE state -- epoch
    /// milliseconds (`System/currentTimeMillis`'s own unit), which is
    /// all `.toEpochMilli` needs to answer honestly. `.truncatedTo`
    /// (real `Instant.truncatedTo(ChronoUnit)`) is a NO-OP identity
    /// here (returns the same instant unchanged) -- the ONE measured
    /// call site (`jsonrpc4clj.trace`'s `format-tag`) only feeds its
    /// result to string formatting for a trace log, which this
    /// campaign's scope never enables/reads.
    Instant(i64),
    /// kondo-wave: `(ReentrantLock.)`'s real state -- an `Arc`-shared
    /// inner `Mutex<ReentrantLockState>` + `Condvar` pair, separate from
    /// this outer `HostInstVal::state` mutex, so `call_reentrant_lock_
    /// method`'s blocking `.lock()` can wait on the inner `Condvar`
    /// (releasing only the INNER mutex, per `Condvar::wait`'s contract)
    /// without holding the outer one.
    ReentrantLock(Arc<(Mutex<ReentrantLockState>, Condvar)>),
    /// lsp/kondo: `java.util.StringTokenizer`'s ENTIRE state -- the
    /// source string as a char vector (indexing by `char`, not byte, so
    /// `pos` always lands on a char boundary), a cursor `pos`, the
    /// CURRENT delimiter set (`nextToken(String)` mutates it for every
    /// later call, so it cannot be baked in at construction like
    /// `BufferedReader`'s eager lines are), and whether delimiters are
    /// themselves returned as one-char tokens. Tokenizing stays lazy
    /// (computed on each call from `pos` forward) rather than
    /// precomputed, exactly because of that `nextToken(String)` mutation.
    StringTokenizer { chars: Vec<char>, pos: usize, delims: Str, return_delims: bool },
    /// mova campaign (clojure-lsp): a `MessageDigest`'s ENTIRE state --
    /// just the algorithm name it was obtained with (`"MD5"`/`"SHA-256"`,
    /// the only two `MessageDigest/getInstance` call sites in the
    /// clojure-lsp+clj-kondo corpus). No `.update` call site exists in
    /// that corpus (both sites call `.digest(bytes)` directly, one-shot),
    /// so unlike real `MessageDigest` this holds no incremental buffer.
    MessageDigest(Str),
    /// lsp/kondo: a `JarFile`/`ZipFile`'s ENTIRE state -- the open
    /// `zip::ZipArchive` (central directory already read: cheap, no
    /// entry data inflated) plus a `closed` flag so `.close`/`with-open`
    /// make later `.entries`/`.getEntry`/`.getInputStream` calls fail
    /// the way a real closed `JarFile` would.
    JarFile { archive: JarArchive, closed: bool },
    /// lsp/kondo: one `JarFile$JarFileEntry`/`ZipEntry` -- just the
    /// central-directory metadata (`.getName`/`.isDirectory`/`.getSize`
    /// read straight off this), plus the archive index `.getInputStream`
    /// needs to inflate this ONE entry's bytes on demand.
    JarEntry { index: usize, name: Str, is_dir: bool, size: u64 },
    /// lsp/kondo: `(.entries jar)`'s `Enumeration` -- the entry list is
    /// collected once (from the cheap raw scan, no inflation) and walked
    /// by `enumeration-seq`'s generic `.hasMoreElements`/`.nextElement`.
    JarEntries { entries: Vec<Value>, pos: usize },
    /// e2: RandomAccessFile/FileChannel share one open file; `None` once closed.
    OpenFile(Option<Arc<std::fs::File>>),
    /// e2: a held fcntl write lock on `file` (whole file); `None` once released.
    FileLock(Option<Arc<std::fs::File>>),
    /// e2: a `java.net.URL`'s spelling.
    Url(Str),
    /// e2: an `HttpsURLConnection`; `resp` is filled by the first `.connect`/`.getResponseCode`/... call.
    HttpConn { url: Str, connect_ms: Option<u64>, read_ms: Option<u64>, resp: Option<HttpResp> },
}

/// e2: a received response (manual `Debug`, same reason as `BoxedReader`).
pub struct HttpResp {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Option<Box<dyn std::io::Read + Send>>,
}

impl std::fmt::Debug for HttpResp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<http-response {}>", self.status)
    }
}

/// Manual `Debug` newtype for `zip::ZipArchive<File>`, same rationale as
/// `BoxedReader`/`BoxedWriter` above (the crate type isn't `Debug`).
pub struct JarArchive(pub zip::ZipArchive<std::fs::File>);

impl std::fmt::Debug for JarArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<jar-archive>")
    }
}

/// Manual `Debug` for a boxed, buffered `Read + Send` stream -- see
/// `HostState::InputStream`'s doc for why this newtype exists at all
/// (a bare `BufReader<Box<dyn Read + Send>>` field would make `#[derive
/// (Debug)]` on `HostState` fail: trait objects have no `Debug`).
pub struct BoxedReader(pub std::io::BufReader<Box<dyn std::io::Read + Send>>);

impl std::fmt::Debug for BoxedReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<input-stream>")
    }
}

/// Manual `Debug` counterpart to `BoxedReader`, same rationale.
/// What a closed input stream reads from: every read fails, as on the JVM.
struct ClosedReader;
impl std::io::Read for ClosedReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("Stream closed"))
    }
}

pub struct BoxedWriter(pub std::io::BufWriter<Box<dyn std::io::Write + Send>>);

impl std::fmt::Debug for BoxedWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<output-stream>")
    }
}

/// C3c: `(Thread. f)`'s cell -- see `HostState::RealThread`'s doc.
#[derive(Debug)]
pub struct RealThreadState {
    /// `Thread.getId` / `getName` as the JVM prints them (`Thread[#31,Thread-2,5,main]`).
    pub id: u64,
    pub name: String,
    pub f: Value,
    pub started: bool,
    pub cell: Arc<crate::value::FutureCell>,
}

/// `java.util.Random`'s ENTIRE state: the 48-bit LCG seed, already
/// scrambled (see `scramble`) -- every method mutates this one `u64` in
/// place, matching the real class's single `AtomicLong seed` field.
#[derive(Debug, Clone, Copy)]
pub struct RandomState {
    pub seed: u64,
}

#[derive(Debug, Clone)]
pub struct ThreadLocalState {
    /// `None` == "not yet computed for this (single, mova-modeled) thread"
    /// -- the state `.remove()` resets back to, so the NEXT `.get` re-runs
    /// `init_fn` (measured against the real JVM, see this module's doc).
    pub value: Option<Value>,
    /// The `proxy`'s `initialValue` override, a 0-arity callable
    /// (`Value::Fn`) -- `None` for a plain (non-proxied) `ThreadLocal`,
    /// whose `.get()` default is `nil` (real Java: `null`) forever, never
    /// memoized (there's nothing to memoize).
    pub init_fn: Option<Value>,
}

// ---------------------------------------------------------------------
// java.util.Random -- the JVM's exact 48-bit LCG (javadoc algorithm).
// ---------------------------------------------------------------------

const MULTIPLIER: u64 = 0x5DEECE66D;
const ADDEND: u64 = 0xB;
const MASK48: u64 = (1u64 << 48) - 1;

/// The constructor/`setSeed` seed scramble: `(initialSeed ^ 0x5DEECE66D) &
/// ((1<<48)-1)`.
fn scramble(initial_seed: i64) -> u64 {
    ((initial_seed as u64) ^ MULTIPLIER) & MASK48
}

/// `next(bits)`: advances the LCG one step and returns the top `bits` bits
/// of the new 48-bit state as a signed 32-bit int -- exactly `(seed >>>
/// (48 - bits))` cast to `int` on the JVM (the cast is a raw bit
/// reinterpretation, which `as u32 as i32` reproduces: `seed >> (48 -
/// bits)` can never exceed `bits <= 32` significant bits, so it always
/// fits a `u32` losslessly before the sign-reinterpreting final cast).
fn next_bits(seed: &mut u64, bits: u32) -> i32 {
    *seed = seed.wrapping_mul(MULTIPLIER).wrapping_add(ADDEND) & MASK48;
    (*seed >> (48 - bits)) as u32 as i32
}

fn next_int(seed: &mut u64) -> i32 {
    next_bits(seed, 32)
}

/// `nextInt(bound)`: the power-of-two fast path, else the javadoc's
/// rejection loop -- both branches use WRAPPING `i32` arithmetic
/// throughout (`bits - val + (bound-1) < 0` deliberately relies on signed
/// overflow the same way the JVM's `int` arithmetic silently does).
fn next_int_bound(seed: &mut u64, bound: i32, span: Span) -> Result<i32, RjError> {
    if bound <= 0 {
        return Err(
            RjError::type_err("nextInt: bound must be positive".to_string()).with_span(span),
        );
    }
    if (bound & bound.wrapping_neg()) == bound {
        // Power of two: `(int)((bound * (long)next(31)) >> 31)`.
        let r31 = next_bits(seed, 31) as i64;
        return Ok(((bound as i64).wrapping_mul(r31) >> 31) as i32);
    }
    loop {
        let bits = next_bits(seed, 31);
        let val = bits.wrapping_rem(bound);
        if bits.wrapping_sub(val).wrapping_add(bound - 1) >= 0 {
            return Ok(val);
        }
    }
}

/// `nextLong()`: `((long)(next(32)) << 32) + next(32)` -- the two
/// `next(32)` calls happen in THIS order (the high half is drawn first,
/// exactly like the JVM's left-to-right operand evaluation).
fn next_long(seed: &mut u64) -> i64 {
    let hi = next_bits(seed, 32) as i64;
    let lo = next_bits(seed, 32) as i64;
    (hi << 32).wrapping_add(lo)
}

/// `nextDouble()`: `(((long)(next(26)) << 27) + next(27)) * (1.0 /
/// (1L << 53))` -- again, `next(26)` before `next(27)`.
fn next_double(seed: &mut u64) -> f64 {
    let hi = next_bits(seed, 26) as i64;
    let lo = next_bits(seed, 27) as i64;
    (((hi << 27) + lo) as f64) * (1.0 / (1u64 << 53) as f64)
}

/// `nextFloat()`: `next(24) / ((float) (1 << 24))` -- computed in `f32`
/// (real JVM float precision) and widened to `f64` only at the very end,
/// so the division itself rounds the same way the JVM's does.
fn next_float(seed: &mut u64) -> f64 {
    (next_bits(seed, 24) as f32 / (1u32 << 24) as f32) as f64
}

fn next_boolean(seed: &mut u64) -> bool {
    next_bits(seed, 1) != 0
}

/// Time-derived seed for the no-arg constructor. Deliberately NOT the
/// JVM's exact `seedUniquifier() ^ System.nanoTime()` algorithm -- no
/// corpus form can pin an unseeded `Random`'s output either way (see this
/// module's doc), so only "successive no-arg constructions don't collide"
/// matters, which a nanosecond clock mixed with a monotonic counter
/// already guarantees.
fn time_seed() -> i64 {
    static COUNTER: AtomicI64 = AtomicI64::new(0);
    // L5/W3 fence #8 (design §4): in sim the seed comes from the seeded USER
    // stream instead of the wall clock, so `(java.util.Random.)` -- which is
    // otherwise the one `Random` whose sequence nothing can pin -- is a
    // function of `MOVA_SIM_SEED` alone. Successive no-arg constructions
    // still don't collide (the stream advances on every draw), which is the
    // only property this seed ever promised.
    if crate::clock::sim_enabled() {
        return crate::clock::user_next() as i64;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos ^ c.wrapping_mul(0x2545_F491_4F6C_DD1Du64 as i64)
}

fn now_millis() -> i64 {
    crate::clock::clock_epoch_ms() as i64
}

// ---------------------------------------------------------------------
// Construction.
// ---------------------------------------------------------------------

pub fn mk_random(initial_seed: i64) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Random,
        state: Mutex::new(HostState::Random(RandomState {
            seed: scramble(initial_seed),
        })),
    }))
}

pub fn mk_date(millis: i64) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Date,
        state: Mutex::new(HostState::Date(millis)),
    }))
}

/// The epoch milliseconds inside a `HostKind::Date` cell, or `None` for
/// any other value -- the one accessor `inst?`/`inst-ms`/the printer share
/// so none of them has to reach into `HostState` itself (SPEC-W1 task 4).
pub fn date_millis(v: &Value) -> Option<i64> {
    let Value::HostInst(h) = v else { return None };
    if h.kind != HostKind::Date {
        return None;
    }
    let guard = crate::sync::lock_mutex(&h.state);
    match &*guard {
        HostState::Date(ms) => Some(*ms),
        _ => None,
    }
}

/// kondo-wave: the path string inside a `HostKind::JavaFile` cell, or
/// `None` for any other value -- `printer.rs`'s one accessor, same
/// "shared read-only accessor, not a whole method surface" shape as
/// `date_millis` above.
pub fn java_file_path(v: &Value) -> Option<crate::value::Str> {
    let Value::HostInst(h) = v else { return None };
    if h.kind != HostKind::JavaFile {
        return None;
    }
    let guard = crate::sync::lock_mutex(&h.state);
    match &*guard {
        HostState::JavaFile(p) => Some(p.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// SPEC-W1 task 4: `#inst` <-> epoch-millis.
//
// `#inst` used to read through `read_tagged_literal`'s permissive
// pass-through, i.e. as a plain STRING (ledgered in
// tests/conformance/pending/reader.corpus). `clojure.spec.alpha`'s
// `inst-in`/`inst-in-range?` are built on `inst?` + `inst-ms`, so the
// literal has to produce a real instant value; mova already HAS one
// (`HostKind::Date`, epoch millis, what `(java.util.Date. n)` builds and
// `inst?` already answers `true` for), so `#inst` reads into that rather
// than minting a second instant shape.
//
// Grammar: `clojure.instant/parse-timestamp`'s own, i.e. the RFC3339
// profile Clojure accepts -- `yyyy[-MM[-dd[THH[:mm[:ss[.fff...]]]]]]`
// with an optional `Z` or `+HH:MM`/`-HH:MM` offset, every omitted field
// defaulting to its lowest legal value. Sub-millisecond digits are
// TRUNCATED (Clojure's own `#inst` -> `java.util.Date` reader does the
// same: a `Date` holds milliseconds and nothing finer).
// ---------------------------------------------------------------------

/// Days since 1970-01-01 for a proleptic-Gregorian `(y, m, d)` --
/// Howard Hinnant's `days_from_civil`, the standard branch-free civil
/// calendar algorithm (public domain). `m` is 1-12, `d` is 1-31.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`] -- Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Number of days in `m` (1-12) of proleptic-Gregorian year `y`.
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Reads exactly `n` ASCII digits off the front of `s`, returning the
/// value and the rest. `None` if fewer than `n` digits are there.
fn take_digits(s: &str, n: usize) -> Option<(i64, &str)> {
    let bytes = s.as_bytes();
    if bytes.len() < n {
        return None;
    }
    let mut acc: i64 = 0;
    for &b in &bytes[..n] {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + i64::from(b - b'0');
    }
    Some((acc, &s[n..]))
}

/// `#inst "..."` -> epoch milliseconds, or `None` for text outside the
/// grammar this module's block comment describes (which is a READER
/// error, exactly as it is on the JVM).
pub fn parse_inst(text: &str) -> Option<i64> {
    let (year, rest) = take_digits(text, 4)?;
    let mut month = 1i64;
    let mut day = 1i64;
    let mut hour = 0i64;
    let mut minute = 0i64;
    let mut second = 0i64;
    let mut millis = 0i64;
    let mut rest = rest;
    if let Some(r) = rest.strip_prefix('-') {
        let (m, r) = take_digits(r, 2)?;
        month = m;
        rest = r;
        if let Some(r) = rest.strip_prefix('-') {
            let (d, r) = take_digits(r, 2)?;
            day = d;
            rest = r;
            if let Some(r) = rest.strip_prefix(['T', 't']) {
                let (h, r) = take_digits(r, 2)?;
                hour = h;
                rest = r;
                if let Some(r) = rest.strip_prefix(':') {
                    let (mi, r) = take_digits(r, 2)?;
                    minute = mi;
                    rest = r;
                    if let Some(r) = rest.strip_prefix(':') {
                        let (s, r) = take_digits(r, 2)?;
                        second = s;
                        rest = r;
                        if let Some(r) = rest.strip_prefix('.') {
                            // One or more fraction digits; only the first
                            // three (milliseconds) survive into a `Date`.
                            let frac: String =
                                r.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if frac.is_empty() {
                                return None;
                            }
                            let mut ms = 0i64;
                            for (i, c) in frac.chars().take(3).enumerate() {
                                let _ = i;
                                ms = ms * 10 + i64::from(c as u8 - b'0');
                            }
                            for _ in frac.chars().take(3).count()..3 {
                                ms *= 10;
                            }
                            millis = ms;
                            rest = &r[frac.len()..];
                        }
                    }
                }
            }
        }
    }
    // Offset: `Z`, `z`, `+HH:MM`, `-HH:MM`, or nothing (= UTC).
    let mut offset_minutes = 0i64;
    if let Some(r) = rest.strip_prefix(['Z', 'z']) {
        rest = r;
    } else if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let (oh, r) = take_digits(&rest[1..], 2)?;
        let r = r.strip_prefix(':')?;
        let (om, r) = take_digits(r, 2)?;
        if oh > 23 || om > 59 {
            return None;
        }
        offset_minutes = (oh * 60 + om) * if sign == '-' { -1 } else { 1 };
        rest = r;
    }
    if !rest.is_empty() {
        return None;
    }
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        // 60 is a leap second -- accepted by `parse-timestamp`, which
        // then normalizes it into the next minute exactly like this
        // arithmetic does.
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs_of_day = hour * 3600 + minute * 60 + second;
    Some(((days * 86_400 + secs_of_day - offset_minutes * 60) * 1000) + millis)
}

/// Epoch milliseconds -> the exact text real Clojure's own `print-method`
/// for `java.util.Date` emits inside `#inst "..."` (measured: the
/// pending-conformance golden for `#inst "2020-01-01"` is `#inst
/// "2020-01-01T00:00:00.000-00:00"`) -- always UTC, always
/// millisecond-precision, always the literal `-00:00` offset spelling.
pub fn format_inst(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let ms_of_day = millis.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s, ms) = (
        ms_of_day / 3_600_000,
        (ms_of_day / 60_000) % 60,
        (ms_of_day / 1000) % 60,
        ms_of_day % 1000,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}-00:00")
}

/// `(java.util.Locale. language)`/`(... language country)`/`(... language
/// country variant)` -- see `HostKind::Locale`'s doc. `country`/`variant`
/// default to `""`, matching real `Locale`'s own constructors.
pub fn mk_locale(language: Str, country: Str, variant: Str) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Locale,
        state: Mutex::new(HostState::Locale { language, country, variant }),
    }))
}

/// `(StringBuilder. ...)`/`(StringBuffer. ...)` -- see `HostKind`'s doc
/// for why this is construction-only.
fn mk_charbuf(kind: HostKind, initial: Str) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind,
        state: Mutex::new(HostState::CharBuf(initial)),
    }))
}

/// `(java.util.ArrayList. ...)` -- see `HostKind::ArrayList`'s doc for why
/// this is construction-only.
fn mk_arraylist(items: crate::value::PVec) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::ArrayList,
        state: Mutex::new(HostState::ArrayList(items)),
    }))
}

/// `java.util.StringTokenizer`'s JDK-default delimiter set (whitespace).
const DEFAULT_TOKENIZER_DELIMS: &str = " \t\n\r\x0c";

/// `(java.util.StringTokenizer. str)`/`(... str delims)`/`(... str delims
/// return-delims?)` -- see `HostState::StringTokenizer`'s doc.
fn mk_string_tokenizer(str: Str, delims: Str, return_delims: bool) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::StringTokenizer,
        state: Mutex::new(HostState::StringTokenizer {
            chars: str.chars().collect(),
            pos: 0,
            delims,
            return_delims,
        }),
    }))
}

/// `(java.util.HashMap. ...)` -- see `HostKind::HashMap`'s doc.
fn mk_hashmap(m: PMap) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::HashMap,
        state: Mutex::new(HostState::HashMap(m)),
    }))
}

/// `(java.util.HashSet. ...)` -- see `HostKind::HashSet`'s doc.
fn mk_hashset(s: champ::PersistentHashSet<Value>) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::HashSet,
        state: Mutex::new(HostState::HashSet(s)),
    }))
}

/// `(new Object)` -- see `HostKind::Object`'s doc. Every call mints a
/// fresh `Arc`, so two calls are never `identical?`/`=` (matches real
/// Java: `new Object()` twice is never `==`/`.equals`).
fn mk_object() -> Value {
    Value::HostInst(Arc::new(HostInstVal { kind: HostKind::Object, state: Mutex::new(HostState::Object) }))
}

/// W4-veneer: `(java.io.File. "path")` -- see `HostKind::JavaFile`'s doc.
/// `pub(crate)` (lsp/io) so `builtins::io`'s `clojure.java.io/file` native
/// can construct one directly from a joined path string, same rationale
/// as every other cross-module `pub(crate)` in this file.
pub(crate) fn mk_java_file(path: Str) -> Value {
    Value::HostInst(Arc::new(HostInstVal { kind: HostKind::JavaFile, state: Mutex::new(HostState::JavaFile(path)) }))
}

/// W4-veneer: `(ReflectorTryCatchFixture.)` -- see `HostKind::
/// ReflectorFixture`'s doc.
fn mk_reflector_fixture() -> Value {
    Value::HostInst(Arc::new(HostInstVal { kind: HostKind::ReflectorFixture, state: Mutex::new(HostState::Object) }))
}

/// `.iterator`'s constructor -- see `HostKind::Iterator`'s doc. `remaining`
/// is `Value::Nil` for an empty/exhausted cursor, or a `List`/whatever
/// `Interp::seq_items` returned otherwise -- called from `eval::
/// types_forms::eval_dot_form`'s C10 universal-fallback `.iterator` arm.
pub(crate) fn mk_iterator(remaining: Value) -> Value {
    Value::HostInst(Arc::new(HostInstVal { kind: HostKind::Iterator, state: Mutex::new(HostState::Iterator(remaining)) }))
}

pub fn mk_thread() -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Thread,
        state: Mutex::new(HostState::Thread),
    }))
}

/// The `n` of `Thread-n` (the JVM numbers user threads from 0) and the thread ids.
static THREAD_NAME_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static THREAD_ID_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30);

fn next_thread_id() -> u64 {
    THREAD_ID_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// `Thread.toString()`: `Thread[#31,Thread-2,5,main]`.
pub fn thread_to_string(state: &HostState) -> String {
    match state {
        HostState::RealThread(rt) => format!("Thread[#{},{},5,main]", rt.id, rt.name),
        _ => "Thread[#1,main,5,main]".to_string(),
    }
}

/// `(Thread. f)` -- see `HostState::RealThread`'s doc. `f` is stored, not
/// yet run: `.start` (`call_thread_method`) does the actual spawning.
fn mk_real_thread(f: Value) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Thread,
        state: Mutex::new(HostState::RealThread(RealThreadState {
            id: next_thread_id(),
            name: format!("Thread-{}", THREAD_NAME_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
            f,
            started: false,
            cell: Arc::new(crate::value::FutureCell::pending()),
        })),
    }))
}

/// `(CyclicBarrier. parties)` -- see `HostState::CyclicBarrier`'s doc.
fn mk_barrier(parties: usize) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::CyclicBarrier,
        state: Mutex::new(HostState::CyclicBarrier(Arc::new(std::sync::Barrier::new(parties)))),
    }))
}

/// `init_fn`: the `proxy`'s `initialValue` override (`None` for a plain,
/// un-proxied `ThreadLocal`, whose `.get()` always answers `nil`). See
/// `eval::special_forms`'s `proxy` handler, this module's only caller for
/// the proxied case.
pub fn mk_threadlocal(init_fn: Option<Value>) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::ThreadLocal,
        state: Mutex::new(HostState::ThreadLocal(ThreadLocalState {
            value: None,
            init_fn,
        })),
    }))
}

/// `(new C a b ...)` / `(C. a b ...)` for a BUILTIN (`ClassVal::Builtin`)
/// class name -- called from `eval::types_forms::eval_new` once it's
/// established `class_val` is not a user `defrecord`/`deftype`. Only
/// `java.util.Random`/`java.util.Date`/`java.lang.ThreadLocal` are
/// constructible this way (`Thread` is not -- see this module's doc); any
/// other builtin class name falls through to the same "no constructor
/// interop" error `eval_new` already raised before this task.
///
/// W3f (small-tail sweep): takes `&mut Interp` now (was: no interpreter
/// access at all) SOLELY so the `java.util.ArrayList` arm can force a
/// `Value::Lazy` argument through `builtins::collections::materialize`
/// -- `sequences.clj`'s `reduce-with-varying-impls` builds one via
/// `(java.util.ArrayList. (range 10))` inside a `mapcat` callback (real
/// `ArrayList(Collection)` accepts and drains any `Iterable`, lazy or
/// not; this constructor used to require an already-realized
/// `Vector`/`List`/`TypedVec`, which is what every OTHER in-scope call
/// site happens to pass -- see the arm's own prior doc, now superseded).
pub fn construct(interp: &mut Interp, name: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    match name {
        // W3e-3: `(java.math.MathContext. precision)` /
        // `(java.math.MathContext. precision RoundingMode)`. The result is
        // the plain `{:precision n :rounding-mode "MODE"}` map mova has
        // used as `*math-context*`'s value since S5 (see `core.mova`'s
        // `with-precision`, which binds exactly this shape, and
        // `builtins::numbers::MATH_CONTEXT` for the thread-local the
        // arithmetic actually reads). Making the ctor produce the SAME
        // shape is the whole point: `clojure.test-clojure.vars/
        // test-settable-math-context` does `(set! *math-context*
        // (java.math.MathContext. 8))` and then expects `(+ 3.55555555555555M 1)`
        // to round to 8 digits, so what the ctor returns and what
        // `with-precision` binds must be one thing, not two.
        //
        // The default rounding mode is `HALF_UP`, which is Java's own
        // `MathContext(int)` default (measured: `(with-precision 4 (/ 1M 3M))`
        // => `0.3333M`, and `core.mova`'s `with-precision` has hardcoded
        // the same default since S5).
        //
        // Deviation, deliberate and documented in
        // `tests/conformance/DEVIATIONS.md`: this is a map, so `(class
        // (java.math.MathContext. 8))` says
        // `clojure.lang.PersistentArrayMap` and `instance?` against the
        // class is always false. A real host-instance variant would buy
        // only those two answers, and `*math-context*`'s printed form is
        // ALREADY a documented divergence (real Clojure prints a
        // nondeterministic `#object[java.math.MathContext 0x... ...]` that
        // no corpus line could assert on either way).
        "java.math.MathContext" => {
            let precision = match args.first() {
                Some(Value::Int(n)) if *n >= 0 => *n,
                Some(other) => {
                    return Err(RjError::type_err(format!(
                        "java.math.MathContext: precision must be a non-negative integer, got {}",
                        crate::printer::display_str(other)
                    ))
                    .with_span(span))
                }
                None => {
                    return Err(RjError::arity(
                        "java.math.MathContext: expected 1 or 2 args, got 0",
                    )
                    .with_span(span))
                }
            };
            let mode: Str = match args.get(1) {
                None => "HALF_UP".into(),
                // `(. java.math.RoundingMode HALF_UP)` is what real
                // `with-precision` splices in; mova has no
                // `RoundingMode` enum values, so a name -- as a string,
                // symbol or keyword -- is what reaches here.
                Some(Value::Str(s)) => s.clone(),
                Some(Value::Sym(s)) => s.name.clone(),
                Some(Value::Keyword(k)) => k.text(),
                Some(other) => {
                    return Err(RjError::type_err(format!(
                        "java.math.MathContext: expected a RoundingMode name, got {}",
                        other.type_name()
                    ))
                    .with_span(span))
                }
            };
            if crate::bignum::RoundingMode::parse(&mode).is_none() {
                return Err(
                    RjError::type_err(format!("java.math.MathContext: no such rounding mode: {mode}"))
                        .with_span(span),
                );
            }
            if args.len() > 2 {
                return Err(RjError::arity(format!(
                    "java.math.MathContext: expected 1 or 2 args, got {}",
                    args.len()
                ))
                .with_span(span));
            }
            let mut m = PMap::new();
            m.insert(Value::Keyword("precision".into()), Value::Int(precision));
            m.insert(Value::Keyword("rounding-mode".into()), Value::Str(mode));
            Ok(Value::Map(m))
        }
        "java.util.Random" => match args {
            [] => Ok(mk_random(time_seed())),
            [Value::Int(seed)] => Ok(mk_random(*seed)),
            [other] => Err(RjError::type_err(format!(
                "java.util.Random: expected a long seed, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.util.Random: expected 0 or 1 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // SPEC-W3 (defect ledger D4): `(java.util.UUID. msb lsb)` -- the
        // ONLY public `UUID` constructor, taking the two `long`s of the
        // 128-bit value most-significant first. mova already had
        // `UUID/randomUUID`, `UUID/fromString`, `#uuid` literals and
        // `Value::Uuid`'s packed `u128`; this was the one way of making
        // one that was missing, and `clojure.test.check.generators/uuid`
        // is written as exactly `(java.util.UUID. (bit-or .. ) (bit-or ..))`
        // over two generated longs -- so `(s/gen uuid?)` / `(s/exercise
        // uuid? n)` could not generate at all.
        //
        // The two `i64`s are reinterpreted as unsigned before packing (a
        // Java `long` is signed, the UUID's 128 bits are not), which is
        // what makes the sign-extended halves land where they belong:
        // measured on the oracle, `(str (java.util.UUID. -1 -1))` =>
        // "ffffffff-ffff-ffff-ffff-ffffffffffff" and `(java.util.UUID. 1
        // 2)` => #uuid "00000000-0000-0001-0000-000000000002". No RFC 4122
        // version/variant bits are imposed -- the real constructor does
        // not impose any either (that is `randomUUID`'s job), and
        // test.check's generator sets them itself.
        "java.util.UUID" => match args {
            [Value::Int(msb), Value::Int(lsb)] => Ok(Value::Uuid(std::sync::Arc::new(
                ((*msb as u64 as u128) << 64) | (*lsb as u64 as u128),
            ))),
            [a, b] => Err(RjError::type_err(format!(
                "java.util.UUID: expected two longs, got {} and {}",
                a.type_name(),
                b.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.util.UUID: expected exactly 2 args (mostSigBits, leastSigBits), got {}",
                args.len()
            ))
            .with_span(span)),
        },
        "java.util.Date" => match args {
            [] => Ok(mk_date(now_millis())),
            [Value::Int(millis)] => Ok(mk_date(*millis)),
            [other] => Err(RjError::type_err(format!(
                "java.util.Date: expected a long millis, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.util.Date: expected 0 or 1 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // W4C-NS (`protocols.clj`'s `exercise-literals`, ctor-arity row):
        // real `java.util.Locale` has exactly three public constructors,
        // 1/2/3 `String` args -- no 0-arg, no 4+-arg. The message/wording
        // is oracle-measured verbatim (`compat/w4c-locale-oracle-
        // transcript.txt`): `Class.toString()`'s "class " prefix, then
        // "got N" with the ACTUAL arg count, not a range.
        "java.util.Locale" => match args {
            [Value::Str(language)] => Ok(mk_locale(language.clone(), Str::from(""), Str::from(""))),
            [Value::Str(language), Value::Str(country)] => {
                Ok(mk_locale(language.clone(), country.clone(), Str::from("")))
            }
            [Value::Str(language), Value::Str(country), Value::Str(variant)] => {
                Ok(mk_locale(language.clone(), country.clone(), variant.clone()))
            }
            [a] | [a, _] | [a, _, _] => Err(RjError::type_err(format!(
                "java.util.Locale: expected a String, got {}",
                a.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::other(format!(
                "Unexpected number of constructor arguments to class java.util.Locale: got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // kondo-wave: `(ReentrantLock.)` -- see `HostKind::ReentrantLock`'s
        // doc: a real reentrant mutex, not a no-op.
        "java.util.concurrent.locks.ReentrantLock" => match args {
            [] => Ok(Value::HostInst(Arc::new(HostInstVal {
                kind: HostKind::ReentrantLock,
                state: Mutex::new(HostState::ReentrantLock(Arc::new((
                    Mutex::new(ReentrantLockState::default()),
                    Condvar::new(),
                )))),
            }))),
            _ => Err(RjError::arity(format!(
                "java.util.concurrent.locks.ReentrantLock: expected 0 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // D5 (`clojure.pprint`): a StringWriter is an `(atom "")` -- see
        // the `java.io.StringWriter` row in `types::builtin_classes()`
        // for why that representation and not a new value kind.
        "java.io.StringWriter" => {
            if !args.is_empty() {
                return Err(RjError::arity(format!(
                    "java.io.StringWriter: expected 0 args, got {}",
                    args.len()
                ))
                .with_span(span));
            }
            let mut cell = crate::value::AtomCell::new(Value::Str(crate::value::Str::from("")));
            cell.string_writer = true;
            Ok(Value::Atom(std::sync::Arc::new(cell)))
        }
        "java.lang.ThreadLocal" => {
            if !args.is_empty() {
                return Err(RjError::arity(format!(
                    "java.lang.ThreadLocal: expected 0 args, got {}",
                    args.len()
                ))
                .with_span(span));
            }
            Ok(mk_threadlocal(None))
        }
        // C3c: `(Thread. f)` -- see this module's doc's `Thread` section
        // and `HostState::RealThread`'s doc. Stores `f`; nothing runs
        // until `.start` (`call_thread_method`).
        "java.lang.Thread" => match args {
            [f] => Ok(mk_real_thread(f.clone())),
            _ => Err(RjError::arity(format!("java.lang.Thread: expected 1 arg (a fn), got {}", args.len())).with_span(span)),
        },
        // C3c: `(CyclicBarrier. parties)` -- see this module's doc's
        // `Thread` section and `HostState::CyclicBarrier`'s doc. Real
        // `CyclicBarrier` also has a 2-arg ctor (`parties`, a
        // `barrierAction` `Runnable` run by the last-arriving thread) --
        // not implemented, no vendored call site passes one.
        "java.util.concurrent.CyclicBarrier" => match args {
            [Value::Int(n)] if *n > 0 => Ok(mk_barrier(*n as usize)),
            [Value::Int(n)] => Err(RjError::type_err(format!(
                "java.util.concurrent.CyclicBarrier: parties must be positive, got {n}"
            ))
            .with_span(span)),
            [other] => Err(RjError::type_err(format!(
                "java.util.concurrent.CyclicBarrier: expected an int parties count, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.util.concurrent.CyclicBarrier: expected 1 arg, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // S6 (uuid/uri predicates batch): `(java.net.URI. s)` -- ONLY the
        // single-string-arg constructor (`predicates.clj`'s own
        // `pred-val-table` call site, `(java.net.URI. "http://clojure.
        // org")`, is the sole vendored-suite use). Stores the string
        // VERBATIM as `Value::Uri` -- real `URI` parses/normalizes its
        // argument into scheme/host/path/etc, but nothing in scope here
        // (the `uri?` truth table, `=`, `str`) observes that, only
        // identity-free equality and the original text back via `str`.
        "java.net.URI" => match args {
            [Value::Str(s)] => Ok(Value::Uri(s.clone())),
            [other] => Err(RjError::type_err(format!(
                "java.net.URI: expected a string, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.net.URI: expected exactly 1 arg, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // S6 (predicates.clj batch): `(StringBuilder. s)`/`(StringBuffer.
        // s)` -- `predicates.clj`'s `test-string?-more` deftest constructs
        // both directly (`(new java.lang.StringBuilder "abc")`) only to
        // assert `(not (string? ..))`; see `HostKind`'s doc for why no
        // method beyond construction exists. The 0-arg and int-capacity
        // overloads are accepted too (real `StringBuilder`/`StringBuffer`
        // have both, and rejecting them would be MORE restrictive than
        // the JVM for no reason) -- capacity is a pure allocation hint on
        // the real class with no externally observable effect, so it is
        // simply discarded here (starts, like the 0-arg form, as an empty
        // buffer).
        "java.lang.StringBuilder" => match args {
            [] => Ok(mk_charbuf(HostKind::StringBuilder, Str::from(""))),
            [Value::Str(s)] => Ok(mk_charbuf(HostKind::StringBuilder, s.clone())),
            [Value::Int(_)] => Ok(mk_charbuf(HostKind::StringBuilder, Str::from(""))),
            [other] => Err(RjError::type_err(format!(
                "java.lang.StringBuilder: expected a string or capacity, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.lang.StringBuilder: expected 0 or 1 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        "java.lang.StringBuffer" => match args {
            [] => Ok(mk_charbuf(HostKind::StringBuffer, Str::from(""))),
            [Value::Str(s)] => Ok(mk_charbuf(HostKind::StringBuffer, s.clone())),
            [Value::Int(_)] => Ok(mk_charbuf(HostKind::StringBuffer, Str::from(""))),
            [other] => Err(RjError::type_err(format!(
                "java.lang.StringBuffer: expected a string or capacity, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.lang.StringBuffer: expected 0 or 1 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // C7 (vecveneer): `(new java.util.ArrayList)` (empty) / `(new
        // java.util.ArrayList coll)` (a snapshot COPY, matching real
        // `ArrayList(Collection)`) -- `test-vector-eqv-to-non-counted-
        // types`'s one call site passes a plain vector literal, so only
        // the already-realized `Vector`/`List`/`TypedVec` shapes are
        // handled directly (no `&mut Interp` reaches this fn -- see
        // `eval::types_forms::eval_new`'s call site -- so a `Lazy`/`Map`/
        // `Set`/etc argument, none of which this suite ever passes here,
        // is an honest "not supported" error rather than a half-forced
        // guess).
        "java.util.ArrayList" => match args {
            [] => Ok(mk_arraylist(crate::value::PVec::new())),
            [Value::Vector(items) | Value::List(items)] => Ok(mk_arraylist(items.clone())),
            [Value::TypedVec(tv)] => Ok(mk_arraylist(tv.data.clone())),
            // W3f: any other seqable (a `Lazy`/improper-tail seq, `Set`,
            // `Map`, ...) gets forced through the same walker `into`/
            // `sort`/... already share -- real `ArrayList(Collection)`
            // drains its argument via `Iterable`, never requiring it
            // pre-realized.
            [other] => Ok(mk_arraylist(crate::value::PVec::from_iter(
                crate::builtins::collections::materialize(interp, other)?,
            ))),
            _ => Err(RjError::arity(format!(
                "java.util.ArrayList: expected 0 or 1 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // lsp/kondo: `(StringTokenizer. str)` / `(... str delims)` /
        // `(... str delims returnDelims?)` -- real JDK's three
        // constructors, delimiters defaulting to `DEFAULT_TOKENIZER_
        // DELIMS` and `returnDelims` to `false`.
        "java.util.StringTokenizer" => match args {
            [Value::Str(s)] => Ok(mk_string_tokenizer(s.clone(), Str::from(DEFAULT_TOKENIZER_DELIMS), false)),
            [Value::Str(s), Value::Str(d)] => Ok(mk_string_tokenizer(s.clone(), d.clone(), false)),
            [Value::Str(s), Value::Str(d), Value::Bool(rd)] => {
                Ok(mk_string_tokenizer(s.clone(), d.clone(), *rd))
            }
            _ => Err(RjError::arity(format!(
                "java.util.StringTokenizer: expected (str), (str delims), or (str delims returnDelims?), got {} args",
                args.len()
            ))
            .with_span(span)),
        },
        // C10: `(new java.util.HashMap)` (empty) / `(new java.util.HashMap
        // m)` (a snapshot COPY of an already-realized `Value::Map`) --
        // `test-count`/`test-contains?`/`test-map-entry?`'s call sites,
        // same "no `&mut Interp` here, only already-realized shapes"
        // constraint as `ArrayList` above.
        "java.util.HashMap" => match args {
            [] => Ok(mk_hashmap(PMap::new())),
            [Value::Map(m)] => Ok(mk_hashmap(m.clone())),
            [other] => Err(RjError::type_err(format!(
                "java.util.HashMap: expected an already-realized map, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.util.HashMap: expected 0 or 1 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // C14 (protocols): `(clojure.lang.MapEntry. k v)` -- the ctor
        // spelling `vectors.clj`'s `test-vec-associative` uses (`.entryAt`
        // comparisons); `MapEntry/create` (wave C, `builtins/statics.rs`)
        // already covers the static-factory spelling with the same body.
        "clojure.lang.MapEntry" => match args {
            [a, b] => Ok(Value::MapEntry(crate::value::PVec::pair(a.clone(), b.clone()))),
            _ => Err(RjError::arity(format!(
                "clojure.lang.MapEntry: expected exactly 2 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // C10: `(new java.util.HashSet)` (empty) / `(new java.util.HashSet
        // s)` (a snapshot COPY of an already-realized `Value::Set`) --
        // `test-contains?`'s call sites.
        "java.util.HashSet" => match args {
            [] => Ok(mk_hashset(champ::PersistentHashSet::new())),
            [Value::Set(s)] => Ok(mk_hashset(s.clone())),
            // W3f (small-tail sweep): real `HashSet(Collection)` accepts
            // ANY `Collection` (never set-only) -- `data.clj`'s `diff-
            // test` builds one from a plain vector literal (`(HashSet.
            // [1 2])`), deduping/snapshotting through the same
            // `materialize` walker `ArrayList`'s arm above now uses (see
            // that arm's own doc for why `&mut Interp` reaches this fn at
            // all).
            [other] => Ok(mk_hashset(
                crate::builtins::collections::materialize(interp, other)?
                    .into_iter()
                    .collect(),
            )),
            _ => Err(RjError::arity(format!(
                "java.util.HashSet: expected 0 or 1 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // C10: `(new Object)` -- see `HostKind::Object`'s doc.
        "java.lang.Object" => match args {
            [] => Ok(mk_object()),
            _ => Err(RjError::arity(format!("java.lang.Object: expected 0 args, got {}", args.len())).with_span(span)),
        },
        // W4-veneer (sequences.clj's `test-iteration` file-IO row):
        // `(java.io.File. "path")` -- see `HostKind::JavaFile`'s doc.
        //
        // kondo-wave: added the real 2-arg `File(String parent, String
        // child)` / `File(File parent, String child)` constructors --
        // `clojure.java.io/file` (the clojure-lsp-kondo overlay's shim)
        // is exactly `(reduce #(File. %1 %2) ...)` over its varargs, same
        // as real `clojure.java.io/as-file`'s multi-arg case, and
        // clj-kondo's own `impl/core.clj` calls `(apply io/file cfg-dir
        // root)` the same way. Path join logic lives in
        // `builtins::fileio::join`, not here (see that module's doc).
        "java.io.File" => match args {
            [Value::Str(s)] => Ok(mk_java_file(s.clone())),
            [Value::Str(parent), Value::Str(child)] => {
                Ok(mk_java_file(crate::builtins::fileio::join(parent.as_ref(), child.as_ref()).into()))
            }
            [Value::HostInst(ph), Value::Str(child)] if ph.kind == HostKind::JavaFile => {
                let HostState::JavaFile(parent) = &*crate::sync::lock_mutex(&ph.state) else {
                    unreachable!("HostKind::JavaFile always carries HostState::JavaFile")
                };
                Ok(mk_java_file(crate::builtins::fileio::join(parent.as_ref(), child.as_ref()).into()))
            }
            [other] => Err(RjError::type_err(format!(
                "java.io.File: expected a string path, got {}",
                other.type_name()
            ))
            .with_span(span)),
            [a, b] => Err(RjError::type_err(format!(
                "java.io.File: expected (parent, child) strings/File, got {} and {}",
                a.type_name(),
                b.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!("java.io.File: expected 1 or 2 args, got {}", args.len())).with_span(span)),
        },
        // lsp/kondo: `(JarFile. file-or-path)` / `(ZipFile. file-or-path)`
        // -- both open the same underlying archive; `HostKind::JarFile`
        // doesn't distinguish them (nothing in scope calls a JarFile-only
        // method like `.getManifest`).
        "java.net.URL" => match args {
            [Value::Str(u)] => {
                if !u.contains("://") {
                    return Err(RjError::thrown(mk_exception(
                        "java.net.MalformedURLException",
                        IO_EXCEPTION_ANCESTORS,
                        Some(Value::Str(Str::from(format!("no protocol: {u}")))),
                        None,
                    ))
                    .with_span(span));
                }
                Ok(Value::HostInst(Arc::new(HostInstVal {
                    kind: HostKind::Url,
                    state: Mutex::new(HostState::Url(u.clone())),
                })))
            }
            _ => Err(RjError::arity(format!("{name}: expected 1 String arg, got {} args", args.len())).with_span(span)),
        },
        "java.io.RandomAccessFile" => {
            let (path, mode) = match args {
                [Value::Str(p), Value::Str(m)] => (p.to_string(), m.to_string()),
                [Value::HostInst(ph), Value::Str(m)] if ph.kind == HostKind::JavaFile => {
                    let HostState::JavaFile(p) = &*crate::sync::lock_mutex(&ph.state) else {
                        unreachable!("HostKind::JavaFile always carries HostState::JavaFile")
                    };
                    (p.to_string(), m.to_string())
                }
                _ => {
                    return Err(RjError::type_err(format!(
                        "{name}: expected (path-or-File, mode), got {} args",
                        args.len()
                    ))
                    .with_span(span))
                }
            };
            mk_random_access_file(&path, &mode, span)
        }
        "java.util.jar.JarFile" | "java.util.zip.ZipFile" => match args {
            [Value::Str(path)] => mk_jar_file(path.as_ref(), span),
            [Value::HostInst(ph)] if ph.kind == HostKind::JavaFile => {
                let HostState::JavaFile(path) = &*crate::sync::lock_mutex(&ph.state) else {
                    unreachable!("HostKind::JavaFile always carries HostState::JavaFile")
                };
                mk_jar_file(path.as_ref(), span)
            }
            [other] => Err(RjError::type_err(format!(
                "{name}: expected a String path or File, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!("{name}: expected 1 arg, got {}", args.len())).with_span(span)),
        },
        // W4-veneer (try_catch.clj's `catch-receives-checked-exception-
        // from-eval`): `(java.io.FileReader. path)` -- see `mk_file_not_
        // found`'s doc for scope (only the "path does not exist" arm is
        // measured/modeled; the vendored call site's path is always
        // deliberately bogus).
        "java.io.FileReader" => match args {
            [Value::Str(s)] => {
                if std::path::Path::new(s.as_ref()).exists() {
                    Err(RjError::other(format!(
                        "java.io.FileReader: {s} exists but reading an existing file through this constructor is out of scope (no vendored .read/.close call site)"
                    ))
                    .with_span(span))
                } else {
                    Err(RjError::thrown(mk_file_not_found(s.as_ref())).with_span(span))
                }
            }
            [other] => Err(RjError::type_err(format!(
                "java.io.FileReader: expected a string path, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!("java.io.FileReader: expected exactly 1 arg, got {}", args.len())).with_span(span)),
        },
        // lsp/io (clj-kondo stdin campaign): `(java.io.StringReader. s)` --
        // see `HostKind::StringReader`'s doc. The one real construction
        // site is `with-in-str`'s macroexpansion (`core.mova`).
        "java.io.StringReader" => match args {
            [Value::Str(s)] => Ok(Value::HostInst(Arc::new(HostInstVal {
                kind: HostKind::StringReader,
                state: Mutex::new(HostState::StringReader { content: s.clone(), pos: 0, closed: false }),
            }))),
            [other] => Err(RjError::type_err(format!(
                "java.io.StringReader: expected a string, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.io.StringReader: expected exactly 1 arg, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // lsp/io: `(java.io.PushbackReader. r)` / `(clojure.lang.
        // LineNumberingPushbackReader. r)` -- both are IDENTITY
        // passthroughs onto the wrapped `HostKind::StringReader` (see
        // `HostKind::StringReader`'s doc for why: `with-in-str`'s
        // macroexpansion wraps a fresh `StringReader` in exactly one of
        // these, and nothing in scope needs push-back or line-counting
        // behavior distinct from the reader underneath -- clj-kondo's OWN
        // push-back/line-tracking is its separate, pure-Clojure
        // `toolsreader` machinery over a plain string+index, never a real
        // `Reader` (see `types.rs`'s `PUSHBACK_READER`/
        // `LINE_NUMBERING_PUSHBACK_READER` table rows). Same shape as the
        // `OutputStreamWriter` passthrough just below.
        "java.io.PushbackReader" | "clojure.lang.LineNumberingPushbackReader" => match args {
            [Value::HostInst(rh)] if rh.kind == HostKind::StringReader => {
                Ok(Value::HostInst(rh.clone()))
            }
            [other] => Err(RjError::type_err(format!(
                "{name}: expected a java.io.StringReader, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!("{name}: expected exactly 1 arg, got {}", args.len())).with_span(span)),
        },
        // lsp/host (clojure-lsp-on-Mova campaign): `(java.io.
        // OutputStreamWriter. some-stream)` (`jsonrpc4clj.server`'s
        // `null-output-stream-writer`: a discard-everything `*out*`
        // sink, `(OutputStreamWriter. (proxy [OutputStream] [] (write
        // ...)))`). IDENTITY passthrough of the wrapped stream/proxy --
        // real `OutputStreamWriter` adds character-encoding on top of a
        // byte `OutputStream`, but mova strings are always already
        // UTF-8 text, so there is no encoding step to actually perform;
        // `strings::out_write`'s generic "`*out*` bound to a `Value::
        // Inst` with a `.write` method" dispatch already finds and
        // calls whatever `.write` the wrapped object (here, a `proxy`)
        // defines, so wrapping would only add a layer nothing reads.
        // The 2-arg constructor form (`OutputStreamWriter. stream
        // charsetName)`) is accepted the same way -- the charset arg is
        // simply unused, same rationale.
        "java.io.OutputStreamWriter" => match args {
            [inner] | [inner, _] => Ok(inner.clone()),
            _ => Err(RjError::arity(format!(
                "java.io.OutputStreamWriter: expected 1 or 2 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // lsp/host: `(java.util.concurrent.CancellationException.)` --
        // `promesa.core.mova`'s `cancel!` throws/rejects with one (see
        // that shim's doc), and `jsonrpc4clj.server` both constructs
        // (implicitly, via `p/cancel!`) and `(catch CancellationException
        // ..)`/`(instance? CancellationException ..)` matches one.
        // Measured ancestry: `CancellationException extends
        // IllegalStateException extends RuntimeException extends
        // Exception extends Throwable`.
        "java.util.concurrent.CancellationException" => match args {
            [] => Ok(mk_exception(
                "java.util.concurrent.CancellationException",
                &[
                    "java.lang.IllegalStateException",
                    "java.lang.RuntimeException",
                    "java.lang.Exception",
                    "java.lang.Throwable",
                ],
                None,
                None,
            )),
            _ => Err(RjError::arity(format!(
                "java.util.concurrent.CancellationException: expected 0 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // lsp/host: `(java.util.concurrent.TimeoutException.)` --
        // `jsonrpc4clj.server`'s `PendingRequest.get(timeout, unit)`
        // throws one on a timed-out deref. CHECKED exception (measured
        // JDK ancestry): `TimeoutException extends Exception extends
        // Throwable` -- NOT under `RuntimeException`.
        "java.util.concurrent.TimeoutException" => match args {
            [] => Ok(mk_exception(
                "java.util.concurrent.TimeoutException",
                &["java.lang.Exception", "java.lang.Throwable"],
                None,
                None,
            )),
            _ => Err(RjError::arity(format!(
                "java.util.concurrent.TimeoutException: expected 0 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // W4-veneer (try_catch.clj's `catch-receives-checked-exception-
        // from-reflective-call`): `(ReflectorTryCatchFixture.)` -- see
        // `HostKind::ReflectorFixture`'s doc.
        "clojure.test.ReflectorTryCatchFixture" => match args {
            [] => Ok(mk_reflector_fixture()),
            _ => Err(RjError::arity(format!(
                "clojure.test.ReflectorTryCatchFixture: expected 0 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // S6 (compat veneer, not JVM emulation -- see this arm group's own
        // doc comment on `boxed_ctor` below): `(Long. x)`/`(Float. x)`/...
        // for the ten `java.lang`/`java.math` boxed-numeric/Boolean/
        // Character classes the vendored suite spells out explicitly
        // (`tests/clojure-suite/vendor/numbers.clj`'s own `(Byte.
        // Byte/MAX_VALUE)` family, and the oracle-measured rows this
        // task's brief pinned). Every one of these already has a bare
        // `ClassVal::Builtin` var (`src/types.rs`'s `builtin_classes()`
        // table -- `Short`/`Byte` were ADDED there by this task, the rest
        // pre-existed), so this `match` only needed a new construction
        // ARM, no new class registration.
        // C14 (protocols): `(java.lang.String. s)` -- `test-ctor-literals`'
        // `#java.lang.String["Hi"]`. mova has no separate boxed/unboxed
        // `String` (a `Value::Str` already IS the "primitive" and the
        // "boxed" value both), so this is pure identity on an existing
        // string, matching real `new String(String)`'s observable
        // behavior (a copy on the JVM, but `=`/`pr-str`-indistinguishable
        // from the original either way).
        "java.lang.String" => match args {
            [Value::Str(s)] => Ok(Value::Str(s.clone())),
            [other] => Err(RjError::type_err(format!(
                "java.lang.String: expected a string, got {}",
                other.type_name()
            ))
            .with_span(span)),
            _ => Err(RjError::arity(format!(
                "java.lang.String: expected exactly 1 arg, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        "java.lang.Long" => boxed_ctor(args, span, "Long", |v| int_ctor(v, "Long", i64::MIN, i64::MAX)),
        "java.lang.Integer" => {
            boxed_ctor(args, span, "Integer", |v| int_ctor(v, "Integer", i32::MIN as i64, i32::MAX as i64))
        }
        "java.lang.Short" => boxed_ctor(args, span, "Short", |v| int_ctor(v, "Short", i16::MIN as i64, i16::MAX as i64)),
        "java.lang.Byte" => boxed_ctor(args, span, "Byte", |v| int_ctor(v, "Byte", i8::MIN as i64, i8::MAX as i64)),
        "java.lang.Double" => boxed_ctor(args, span, "Double", |v| float_ctor(v, "Double", false)),
        "java.lang.Float" => boxed_ctor(args, span, "Float", |v| float_ctor(v, "Float", true)),
        "java.lang.Boolean" => boxed_ctor(args, span, "Boolean", boolean_ctor),
        "java.lang.Character" => boxed_ctor(args, span, "Character", character_ctor),
        "java.math.BigDecimal" => boxed_ctor(args, span, "BigDecimal", bigdecimal_ctor),
        // clojure-lsp campaign (mova/PLAN.md): the 2-arg `BigInteger(String
        // val, int radix)` ctor -- `clojure.tools.reader.impl.commons`'s
        // number parser (a transitive dependency of `rewrite-clj.reader`)
        // calls it directly for every non-decimal integer literal
        // (`0x1F`, `2r1010`, octal `017`). 1-arg keeps the existing
        // `biginteger_ctor` path unchanged.
        "java.math.BigInteger" => match args {
            [v] => biginteger_ctor(v).map_err(|e| e.with_span(span)),
            [Value::Str(s), Value::Int(radix)] => BigInt::parse_bytes(s.as_bytes(), *radix as u32)
                .map(|n| Value::BigInteger(Arc::new(BigIntVal(n))))
                .ok_or_else(|| {
                    RjError::type_err(format!("BigInteger: invalid input {s:?} for radix {radix}"))
                        .with_span(span)
                }),
            // mova campaign (clojure-lsp): the 2-arg `BigInteger(int
            // signum, byte[] magnitude)` ctor -- `clojure-lsp.shared/md5`
            // (`(BigInteger. 1 raw)`, `raw` a `MessageDigest.digest`
            // result) calls exactly this overload. `num_bigint::BigInt::
            // from_bytes_be` is the same big-endian-unsigned-magnitude
            // reading, real `BigInteger`'s own ctor requires `signum` in
            // `{-1,0,1}` (an out-of-range value throws
            // `NumberFormatException`, mirrored as a type error here).
            [Value::Int(signum), Value::Array(arr)] if arr.kind == crate::value::ArrayKind::Byte => {
                let sign = match signum {
                    -1 => num_bigint::Sign::Minus,
                    0 => num_bigint::Sign::NoSign,
                    1 => num_bigint::Sign::Plus,
                    other => {
                        return Err(RjError::type_err(format!("BigInteger: invalid signum value {other}"))
                            .with_span(span))
                    }
                };
                let bytes: Vec<u8> = crate::sync::lock_mutex(&arr.data)
                    .iter()
                    .map(|v| match v {
                        Value::Int(n) => *n as u8,
                        _ => 0,
                    })
                    .collect();
                Ok(Value::BigInteger(Arc::new(BigIntVal(BigInt::from_bytes_be(sign, &bytes)))))
            }
            _ => Err(RjError::arity(format!(
                "BigInteger: expected 1 or 2 args, got {}",
                args.len()
            ))
            .with_span(span)),
        },
        // S6/libstatics -- see this arm group's own doc comment (right
        // after `construct`'s `match`, below `exception_ctor`) for the
        // "thin veneer, not a Java exception hierarchy" framing.
        "java.lang.Exception" => {
            exception_ctor(args, span, "java.lang.Exception", EXCEPTION_ANCESTORS)
        }
        "java.lang.RuntimeException" => exception_ctor(
            args,
            span,
            "java.lang.RuntimeException",
            RUNTIME_EXCEPTION_ANCESTORS,
        ),
        "java.lang.IllegalArgumentException" => exception_ctor(
            args,
            span,
            "java.lang.IllegalArgumentException",
            RUNTIME_EXCEPTION_ANCESTORS,
        ),
        "java.lang.IndexOutOfBoundsException" => exception_ctor(
            args,
            span,
            "java.lang.IndexOutOfBoundsException",
            RUNTIME_EXCEPTION_ANCESTORS,
        ),
        // mova/PLAN.md interop-census batch: `IllegalStateException`/
        // `UnsupportedOperationException` -- edamame's syntax-quote
        // impl and rewrite-clj node ctors throw these directly.
        "java.lang.IllegalStateException" => exception_ctor(
            args,
            span,
            "java.lang.IllegalStateException",
            RUNTIME_EXCEPTION_ANCESTORS,
        ),
        "java.lang.UnsupportedOperationException" => exception_ctor(
            args,
            span,
            "java.lang.UnsupportedOperationException",
            RUNTIME_EXCEPTION_ANCESTORS,
        ),
        // nREPL gaps: the classes an internal error now catch-binds as
        // (see `errinfo`), constructible and `instance?`-testable too.
        "java.lang.ArithmeticException" => {
            exception_ctor(args, span, "java.lang.ArithmeticException", RUNTIME_EXCEPTION_ANCESTORS)
        }
        "java.lang.ClassCastException" => {
            exception_ctor(args, span, "java.lang.ClassCastException", RUNTIME_EXCEPTION_ANCESTORS)
        }
        "java.lang.NullPointerException" => {
            exception_ctor(args, span, "java.lang.NullPointerException", RUNTIME_EXCEPTION_ANCESTORS)
        }
        "java.lang.NumberFormatException" => {
            exception_ctor(args, span, "java.lang.NumberFormatException", ARITY_EXCEPTION_ANCESTORS)
        }
        // S6 (assert/namespace/uuid batch): `core.mova`'s `assert` macro
        // needs a real `AssertionError` to throw -- same thin-veneer
        // shape as the four exceptions above, but `AssertionError`
        // extends `java.lang.Error`, NOT `RuntimeException`/`Exception`
        // (measured: `(instance? Error (AssertionError. "x"))` => true,
        // `(instance? Exception (AssertionError. "x"))` => false), hence
        // its own ancestor list rather than reusing
        // `RUNTIME_EXCEPTION_ANCESTORS`.
        "java.lang.AssertionError" => {
            exception_ctor(args, span, "java.lang.AssertionError", ASSERTION_ERROR_ANCESTORS)
        }
        // Wave-C small sweep item 5 (errors.clj's `ex-info-allows-nil-
        // data`, `(ex-info "message" nil (Throwable. "cause"))`): bare
        // `Throwable.` was previously a class VAR only (for `instance?
        // Throwable`, see `register`'s own comment) with no constructor
        // arm -- every OTHER row in this task's scan needed a subclass,
        // never the root itself, until now. Same shape as the other four,
        // ancestors empty: `Throwable` has nothing above it in this
        // module's chain (real `java.lang.Object` isn't tracked here).
        "java.lang.Throwable" => exception_ctor(args, span, "java.lang.Throwable", &[]),
        // W4-PRINTER: bare `java.lang.Error` had a class VAR
        // (`register`'s `classes` table, for `instance? Error`) but no
        // constructor arm -- `AssertionError` (a subclass) was the only
        // `Error`-family row constructible before now. Same shape as the
        // other exception ctors, `ERROR_ANCESTORS` above.
        "java.lang.Error" => exception_ctor(args, span, "java.lang.Error", ERROR_ANCESTORS),
        // `(throw (StackOverflowError.))`: `VirtualMachineError <: Error <: Throwable`.
        "java.lang.StackOverflowError" => exception_ctor(args, span, "java.lang.StackOverflowError", STACK_OVERFLOW_ANCESTORS),
        // W4-EVAL task 4: `core.mova`'s `check-transient!` needs a real
        // `IllegalAccessError` to throw for a stale transient handle
        // (measured message: "Transient used after persistent! call").
        // Same thin-veneer shape as the exceptions above, `Error`-rooted
        // ancestry like `AssertionError` (see `ILLEGAL_ACCESS_ERROR_
        // ANCESTORS`'s own doc for exactly how far down that branch).
        "java.lang.IllegalAccessError" => exception_ctor(
            args,
            span,
            "java.lang.IllegalAccessError",
            ILLEGAL_ACCESS_ERROR_ANCESTORS,
        ),
        // W3f: `clojure.lang.Compiler$CompilerException` -- see
        // `COMPILER_EXCEPTION_ANCESTORS`'s own doc.
        "clojure.lang.Compiler$CompilerException" => exception_ctor(
            args,
            span,
            "clojure.lang.Compiler$CompilerException",
            COMPILER_EXCEPTION_ANCESTORS,
        ),
        other => Err(RjError::other(format!(
            "new: cannot construct builtin class {other} (no constructor interop)"
        ))
        .with_span(span)),
    }
}

// ---------------------------------------------------------------------
// S6/libstatics: exception constructors -- `(IllegalArgumentException.
// "msg")`, `(RuntimeException.)`, `(Exception. "msg")`,
// `(IndexOutOfBoundsException.)`.
//
// COMPAT VENEER, NOT A JAVA EXCEPTION HIERARCHY (same framing as the
// boxed-numeric ctors above): the vendored suite's own `throw` sites
// spell these four Java exception idioms directly
// (`tests/clojure-suite/vendor/{control,errors,delays,for,test,vars,rt}.
// clj`, `tests/clojure-suite/vendor-libs/clojure/test/{generative.clj,
// check/rose_tree.cljc}`), and `clojure.test.check.properties`
// (vendor-libs) checks `(instance? Throwable x)` on a caught value to
// tell a thrown failure from an ordinary `false` property result -- so
// this needs enough of the REAL `java.lang.Throwable` hierarchy
// (`IllegalArgumentException`/`IndexOutOfBoundsException` <:
// `RuntimeException` <: `Exception` <: `Throwable`, measured against
// `.oracle`: `(instance? RuntimeException (IllegalArgumentException.
// "x"))` => `true`, `(instance? Throwable (Exception. "x"))` => `true`)
// for `instance?` against a SUPERCLASS to answer correctly, not the full
// JVM class-hierarchy machinery.
//
// Modeled as an ordinary (non-record) `Value::Inst` -- `TypeDef`/
// `InstVal` are already `pub` in `crate::types`, constructed directly
// here rather than through `deftype`'s special form. ONE basis field,
// named LITERALLY `"getMessage"`, so `(.getMessage e)` resolves through
// the EXISTING deftype field-access dot-dispatch in
// `eval::types_forms::eval_dot_form` (`inst_field`'s non-record arm:
// `tdef.basis.iter().position(|b| b.as_ref() == field)`) with NO changes
// needed there -- a deliberate reuse of a mechanism this module doesn't
// own, rather than a new one. The ancestor CHAIN (everything the
// instance should also answer `instance?` true for) rides
// `TypeDef::interfaces` -- that field's own doc describes it as
// `definterface`/host-interface NAMES a type declared, but nothing about
// its `Vec<Str>` "names this instance answers `instance?` true for"
// shape is definterface-specific, and repurposing it here needed no
// edit to `types.rs` (unlike a hypothetical dedicated ancestor field
// would have). Each exception class is registered (`register`, below)
// as an ordinary `ClassVal::Builtin` with a `pred` closure that checks
// `tdef.name` OR `tdef.interfaces` for its own name -- see
// `is_exception_named`.
//
// Deliberately narrow: no `cause`-argument (2-arity) constructor (none
// of the vendored `throw` sites reachable by this task's oracle
// measurement pass a cause), no `.getStackTrace`/`.setStackTrace`/
// `.toString` on these instances (measured divergence, not chased -- see
// this task's own final report), and no `ArithmeticException` (the
// vendored suite only ever `catch`es it -- untyped `catch` already
// matches ANY thrown value, see `SHIM-LIMITS.md` -- never constructs
// one).

const RUNTIME_EXCEPTION_ANCESTORS: &[&str] =
    &["java.lang.RuntimeException", "java.lang.Exception", "java.lang.Throwable"];
const EXCEPTION_ANCESTORS: &[&str] = &["java.lang.Exception", "java.lang.Throwable"];
// C3c: `clojure.lang.ArityException` extends `IllegalArgumentException`
// (measured: `(supers clojure.lang.ArityException)` => `#{java.lang.
// RuntimeException java.io.Serializable java.lang.Throwable java.lang.
// Exception java.lang.IllegalArgumentException java.lang.Object}`) --
// NOT `RUNTIME_EXCEPTION_ANCESTORS` directly, one level deeper.
const ARITY_EXCEPTION_ANCESTORS: &[&str] = &[
    "java.lang.IllegalArgumentException",
    "java.lang.RuntimeException",
    "java.lang.Exception",
    "java.lang.Throwable",
];
// S6: `AssertionError`'s real ancestry is `Error <: Throwable` (siblings
// with `Exception`, NOT under it) -- see the `construct` arm's own doc.
const ASSERTION_ERROR_ANCESTORS: &[&str] = &["java.lang.Error", "java.lang.Throwable"];
// W4-PRINTER (printer.clj's `print-throwable` deftest's "mixed" row,
// `(Error. "less outer" (ex-info ...))`): bare `java.lang.Error` itself,
// same shape as `java.lang.Throwable`'s own bare-root row above -- extends
// `Throwable` directly (measured: `(supers java.lang.Error)` =>
// `#{java.lang.Throwable}`), NOT `Exception`.
const ERROR_ANCESTORS: &[&str] = &["java.lang.Throwable"];
const STACK_OVERFLOW_ANCESTORS: &[&str] = &["java.lang.VirtualMachineError", "java.lang.Error", "java.lang.Throwable"];
// W3a + W3f (merged): `clojure.lang.Compiler$CompilerException extends
// RuntimeException` (measured via `.getSuperclass`). Two legitimate mint
// paths, both engine-internal: W3a's `mk_compiler_exception` (an error the
// evaluator itself raised while compiling a form -- the only way one comes
// into existence on the JVM either), and W3f's `case` duplicate-test-
// constant construct arm (`control.clj`'s `(thrown-with-cause-msg?
// clojure.lang.Compiler$CompilerException #"Duplicate case test constant"
// ...)` -- the JVM's own case macroexpansion throws exactly this). Script
// code still cannot construct one from an arbitrary ctor call.
const COMPILER_EXCEPTION_ANCESTORS: &[&str] = &[
    "java.lang.RuntimeException",
    "java.lang.Exception",
    "java.lang.Throwable",
];
// W4-EVAL task 4 (`transients.clj`'s `transient-mod-after-persistent`
// deftest): `IllegalAccessError`'s real ancestry is `IncompatibleClass
// ChangeError <: LinkageError <: Error <: Throwable` (measured) -- an
// `Error`, NOT an `Exception`, same shape distinction `AssertionError`
// above already needed, just one link further down its own separate
// branch (`Error` <: `Throwable` directly for `AssertionError`; here
// `Error` has two more real JVM classes between it and this one).
const ILLEGAL_ACCESS_ERROR_ANCESTORS: &[&str] = &[
    "java.lang.IncompatibleClassChangeError",
    "java.lang.LinkageError",
    "java.lang.Error",
    "java.lang.Throwable",
];

/// Builds one exception instance. `ancestors` should include `class_name`
/// itself is NOT repeated here -- `is_exception_named` checks `tdef.name`
/// (the class's own name) OR `tdef.interfaces` (everything ABOVE it in
/// the chain) separately, so `ancestors` lists strict superclasses only.
///
/// C3c (errors.clj's `Throwable->map-test` "causes" sub-test): `cause`
/// rides a SECOND basis field, `"getCause"` -- same field-position-dot-
/// dispatch mechanism `"getMessage"` already uses, so `.getCause` on any
/// of these instances resolves through the ordinary deftype field-access
/// path with no new machinery. `Value::Nil` (never constructed with one)
/// for every exception built before this task's 2-arg ctor existed.
fn mk_exception(class_name: &'static str, ancestors: &'static [&'static str], message: Option<Value>, cause: Option<Value>) -> Value {
    let tdef = Arc::new(crate::types::TypeDef {
        name: class_name.into(),
        basis: vec!["getMessage".into(), "getCause".into()],
        is_record: false,
        interfaces: ancestors.iter().map(|&s| s.into()).collect(),
        field_tags: Vec::new(),
            mutable: Vec::new(),
        methods: Default::default(),
        protocols: Vec::new(),
    });
    Value::Inst(Arc::new(crate::types::InstVal {
        tdef,
        data: crate::value::PMap::new(),
        fields: std::sync::Mutex::new([message.unwrap_or(Value::Nil), cause.unwrap_or(Value::Nil)].into_iter().collect()),
        meta: None,
    }))
}

// W4-veneer (try_catch.clj's `catch-receives-checked-exception-from-eval`):
// `java.io.FileNotFoundException` -- measured on the oracle, `(ancestors
// java.io.FileNotFoundException)` => `#{java.io.IOException
// java.lang.Exception java.lang.Throwable java.lang.Object}` (NOT under
// `RuntimeException` -- `FileNotFoundException` is a CHECKED exception,
// same "measure before assuming RuntimeException" caution `mk_exception`'s
// other callers already take).
const FILE_NOT_FOUND_ANCESTORS: &[&str] =
    &["java.io.IOException", "java.lang.Exception", "java.lang.Throwable"];

/// W4-veneer: `(java.io.FileReader. path)` on a path that does not exist
/// -- the ONE vendored call site (`try_catch.clj`'s
/// `catch-receives-checked-exception-from-eval`, `(java.io.FileReader.
/// "CAFEBABEx0/idonotexist")`) always opens a deliberately-bogus path, so
/// only the "does not exist" arm is measured/modeled; a `FileReader` on a
/// path that DOES exist has no vendored consumer (no `.read`/`.close`
/// call site) and is out of scope.
pub(crate) fn mk_file_not_found(path: &str) -> Value {
    mk_exception(
        "java.io.FileNotFoundException",
        FILE_NOT_FOUND_ANCESTORS,
        Some(Value::Str(Str::from(format!("{path} (No such file or directory)")))),
        None,
    )
}

// W4-veneer (try_catch.clj's `catch-receives-checked-exception-from-
// reflective-call`): `clojure.test.ReflectorTryCatchFixture$Cookies` --
// `.oracle/clojure-src/test/java/clojure/test/ReflectorTryCatchFixture.java`:
// `public static class Cookies extends Exception`, i.e. directly under
// `Exception`, same ancestor shape as `mk_exception`'s own
// `EXCEPTION_ANCESTORS`.
const COOKIES_ANCESTORS: &[&str] = &["java.lang.Exception", "java.lang.Throwable"];

/// Builds one `Cookies` instance -- see `call_reflector_fixture_method`/
/// `reflector_fixture_fail`'s docs for the two call sites and exact
/// messages/causes the vendored corpus needs.
pub(crate) fn mk_cookies(message: &'static str, cause: Option<Value>) -> Value {
    mk_exception(
        "clojure.test.ReflectorTryCatchFixture$Cookies",
        COOKIES_ANCESTORS,
        Some(Value::Str(Str::from(message))),
        cause,
    )
}

/// C3c (errors.clj's `arity-exception` deftest): builds a real `clojure.
/// lang.ArityException` instance for an `ErrorKind::Arity` `RjError` that
/// populated `arity_actual` (`eval::special_forms::error_to_info_map`'s
/// only caller). `actual` rides an ordinary basis field (`"actual"`,
/// accessed via `.-actual` -- field-position dot-dispatch, same mechanism
/// `mk_exception`'s `"getMessage"` field already uses for method-position
/// access), matching real `clojure.lang.ArityException.actual`'s `int`
/// field exactly (measured via `.getFields`: the class has exactly TWO
/// public fields, `actual int` and `name String` -- `name` is NOT
/// modeled here, nothing in scope reads it, see `RjError::arity_actual`'s
/// doc).
/// `cause`: `.getCause`'s value -- `Value::Nil` for the overwhelming
/// majority of arity errors (see `RjError::arity_cause`'s doc for the
/// one call site that populates it with something real).
pub(crate) fn mk_arity_exception(actual: i64, message: impl Into<crate::value::Str>, cause: Value) -> Value {
    let tdef = Arc::new(crate::types::TypeDef {
        name: "clojure.lang.ArityException".into(),
        basis: vec!["actual".into(), "getMessage".into(), "getCause".into()],
        is_record: false,
        interfaces: ARITY_EXCEPTION_ANCESTORS.iter().map(|&s| s.into()).collect(),
        field_tags: Vec::new(),
            mutable: Vec::new(),
        methods: Default::default(),
        protocols: Vec::new(),
    });
    Value::Inst(Arc::new(crate::types::InstVal {
        tdef,
        data: crate::value::PMap::new(),
        fields: std::sync::Mutex::new([Value::Int(actual), Value::Str(message.into()), cause].into_iter().collect()),
        meta: None,
    }))
}

/// W3a (special.clj's `quote-with-multiple-args`): builds the
/// `clojure.lang.Compiler$CompilerException` a compile-time rejection
/// catch-binds to -- `.getMessage` is mova's own diagnostic text,
/// `.getCause` is the condition it wrapped (an `ex-info`-shaped map, the
/// same shape `core.mova`'s `ex-data`/`ex-message`/`Throwable->map` already
/// read). Mirrors real Clojure exactly: `(eval '(quote 1 2 3))` throws a
/// `Compiler$CompilerException` whose cause is a `clojure.lang.ExceptionInfo`
/// carrying `{:form (quote 1 2 3)}`, and the vendored deftest reads
/// `(-> ex (.getCause) (ex-data) (:form))`.
///
/// Same two-field `TypeDef` shape `mk_arity_exception` above uses, for the
/// same reason: `getMessage`/`getCause` ride ordinary basis fields, so
/// `.getMessage`/`.getCause` resolve through the everyday record-field
/// dot-dispatch with no new machinery.
pub(crate) fn mk_compiler_exception(message: impl Into<crate::value::Str>, cause: Value) -> Value {
    let tdef = Arc::new(crate::types::TypeDef {
        name: "clojure.lang.Compiler$CompilerException".into(),
        basis: vec!["getMessage".into(), "getCause".into()],
        is_record: false,
        interfaces: COMPILER_EXCEPTION_ANCESTORS.iter().map(|&s| s.into()).collect(),
        field_tags: Vec::new(),
            mutable: Vec::new(),
        methods: Default::default(),
        protocols: Vec::new(),
    });
    Value::Inst(Arc::new(crate::types::InstVal {
        tdef,
        data: crate::value::PMap::new(),
        fields: std::sync::Mutex::new([Value::Str(message.into()), cause].into_iter().collect()),
        meta: None,
    }))
}

/// W4-EVAL task 1 (`evaluation.clj`'s `SymbolResolution` deftest): the
/// `.getCause` object real Clojure's compiler wraps around a qualified
/// read of another namespace's `:private` var (`Compiler.java`'s
/// `resolveIn`, measured: `IllegalStateException("var: " + sym + " is not
/// public")`), meant as `mk_compiler_exception`'s second argument (see
/// `ns::Interp::check_qualified_private`, the sole caller). Real ancestry
/// is plain `IllegalStateException <: RuntimeException <: Exception <:
/// Throwable` -- `RUNTIME_EXCEPTION_ANCESTORS` already lists exactly that
/// (one level up), so it's reused rather than adding a near-duplicate
/// const for one call site.
pub(crate) fn mk_illegal_state_exception(message: impl Into<crate::value::Str>) -> Value {
    mk_exception(
        "java.lang.IllegalStateException",
        RUNTIME_EXCEPTION_ANCESTORS,
        Some(Value::Str(message.into())),
        None,
    )
}

/// M8 slice 1 / defect D13: builds a real host exception `Value::Inst` for
/// an `error::JvmClass` -- the class an internal `RjError` was
/// oracle-MEASURED to present as (see `JvmClass`'s own doc: every variant
/// is tagged at a site that checked what real Clojure 1.13.0-alpha6 raises
/// for exactly that condition). `eval::special_forms::error_to_info_map`'s
/// generalized arm is the only caller.
///
/// The class name is `chain()[0]` and the ancestors are its tail --
/// `JvmClass::chain` already returns "own class first, then each
/// superclass up to `Throwable`", which is precisely `mk_exception`'s
/// `(class_name, ancestors)` pair, so the two tables cannot drift: a typed
/// `catch` matching on `chain()` and the value that same `catch` BINDS are
/// built from one constant. Same two-field `["getMessage" "getCause"]`
/// basis `mk_exception`'s other callers use, so `.getMessage`/`.getCause`
/// resolve through the everyday record-field dot-dispatch with no new
/// machinery, and `instance?`/`Throwable->map`/`pr-str`'s `#error {...}`
/// all already understand the shape.
///
/// `cause` is `Value::Nil` at the generalized call site (an internal error
/// carries no cause chain -- the one internal error that does,
/// `eval_quote`'s `Compiler$CompilerException`, is minted by
/// `mk_compiler_exception` in an earlier arm and never reaches here).
#[allow(dead_code)]
pub(crate) fn mk_jvm_exception(
    class: crate::error::JvmClass,
    message: impl Into<crate::value::Str>,
    cause: Value,
) -> Value {
    let chain = class.chain();
    mk_exception(chain[0], &chain[1..], Some(Value::Str(message.into())), Some(cause))
}

/// Shared ctor body for all six exception classes (was four -- see this
/// arm group's doc for `AssertionError`/`Throwable` joining the original
/// four): 0 args (`.getMessage` => `nil`, measured: `(.getMessage
/// (IndexOutOfBoundsException.))` => `nil`), 1 string-message arg
/// (`.getMessage` => that string, measured: `(.getMessage
/// (RuntimeException. "rt-msg"))` => `"rt-msg"`), or -- C3c (errors.clj's
/// `Throwable->map-test` "causes" sub-test) -- 2 args (message + cause):
/// `.getCause` => the second arg verbatim (measured: `(.getCause
/// (Exception. "msg" (Exception. "inner")))` is that inner exception
/// instance; `(.getCause (Exception. "msg" nil))` is `nil`, same as the
/// 1-arg ctor's implicit cause). Any other arity or first-arg type is an
/// ordinary arity/type `RjError`; the cause arg is NOT type-checked
/// against `Throwable` (compat veneer, not JVM emulation -- see this arm
/// group's own doc) since nothing in scope passes a non-exception, non-
/// nil cause.
fn exception_ctor(
    args: &[Value],
    span: Span,
    class_name: &'static str,
    ancestors: &'static [&'static str],
) -> Result<Value, RjError> {
    match args {
        [] => Ok(mk_exception(class_name, ancestors, None, None)),
        [Value::Str(s)] => Ok(mk_exception(class_name, ancestors, Some(Value::Str(s.clone())), None)),
        [Value::Str(s), cause] => Ok(mk_exception(class_name, ancestors, Some(Value::Str(s.clone())), Some(cause.clone()))),
        [other] => Err(RjError::type_err(format!(
            "{class_name}: expected a string message, got {}",
            other.type_name()
        ))
        .with_span(span)),
        _ => Err(RjError::arity(format!(
            "{class_name}: expected 0, 1, or 2 args, got {}",
            args.len()
        ))
        .with_span(span)),
    }
}

/// The ancestor chain `instance?` attributes to an `ex-info` result --
/// which mova represents as a plain `Value::Map` under `:ex/`-namespaced
/// keys (`core.mova`'s `ex-info`), not a `Value::Inst`. Real Clojure's
/// `clojure.lang.ExceptionInfo` is a measured `RuntimeException` subclass,
/// and `eval::special_forms::thrown_value_class_chain` already gives a
/// thrown one EXACTLY this chain for `catch` -- this constant is that same
/// table, shared with `instance?` (SPEC-W1 task 3).
const EX_INFO_CHAIN: &[&str] = &[
    "clojure.lang.ExceptionInfo",
    "java.lang.RuntimeException",
    "java.lang.Exception",
    "java.lang.Throwable",
];

/// The ancestor chain for a CAUGHT internal error -- `error_to_info_map`'s
/// `{:type :error/<kind> :message ..}` shape, what `catch` binds for every
/// error kind that has no host-exception instance of its own (divide by
/// zero, type errors, unresolved symbols, ...). No SPECIFIC class is
/// claimed: this is the same generic "some kind of runtime exception"
/// tail `error_kind_class_chain`'s own `ErrorKind::Other` arm ends in, and
/// the honest ceiling on what an untyped info map can assert.
const ERROR_INFO_CHAIN: &[&str] = &[
    "java.lang.RuntimeException",
    "java.lang.Exception",
    "java.lang.Throwable",
];

/// SPEC-W1 task 3: the chain for a `Value::Map` that IS one of mova's
/// exception values, or `None` for an ordinary map.
///
/// Two shapes, both minted by mova itself and both already classified this
/// way by `catch`: an `ex-info` result (carries `:ex/message` -- the same
/// discriminator `core.mova`'s `ex-message`/`ex-data`/`Throwable->map`,
/// `types::builtin_class_name` and `thrown_value_class_chain` all use) and
/// a caught internal error (`:type` bound to an `error/`-namespaced
/// keyword, `error_to_info_map`'s own output and nothing else).
fn error_map_chain(m: &crate::value::PMap) -> Option<&'static [&'static str]> {
    if m.get(&Value::Keyword(crate::keyword::Keyword::from("ex/message")))
        .is_some()
    {
        return Some(EX_INFO_CHAIN);
    }
    if is_error_info_map(m) {
        return Some(ERROR_INFO_CHAIN);
    }
    None
}

/// True for `eval::special_forms::error_to_info_map`'s output and nothing
/// else: a map whose `:type` is an `error/`-namespaced keyword. Shared by
/// `error_map_chain` (above) and `eval::types_forms`'s `.getMessage`
/// dot-method arm -- see that arm for why the two must agree.
pub(crate) fn is_error_info_map(m: &crate::value::PMap) -> bool {
    matches!(
        m.get(&Value::Keyword(crate::keyword::Keyword::from("type"))),
        Some(Value::Keyword(k)) if k.as_ref().starts_with("error/")
    )
}

/// `instance?`'s membership test for a synthetic exception class `name`:
/// true iff `v` is one of OUR exception `Value::Inst`s whose class is
/// `name` ITSELF (`tdef.name`) or a subclass of it (`name` appears in
/// `tdef.interfaces`, this module's ancestor-chain repurposing -- see the
/// arm group's doc), OR one of the two `Value::Map` exception shapes
/// `error_map_chain` recognizes.
///
/// SPEC-W1 task 3 added that second half. Before it, `(instance? Throwable
/// (ex-info "x" {}))` was FALSE while `(catch Throwable ..)` caught the
/// same value -- an asymmetry `clojure.spec.alpha`'s `validate-fn` (`(let
/// [ret (try (apply f args) (catch Throwable t t))] (if (instance?
/// Throwable ret) ...))`) and `clojure.test.check.properties`' own
/// `exception?` helper both read as "the call succeeded and returned a
/// map", silently turning a thrown failure into a passing result. The two
/// questions now agree wherever mova mints the value; they still diverge
/// where `catch` is deliberately TOTAL and `instance?` is deliberately not
/// (`(instance? Exception 42)` stays false for a bare thrown number --
/// see `thrown_value_class_chain`'s doc and
/// tests/conformance/DEVIATIONS.md).
fn is_exception_named(v: &Value, name: &str) -> bool {
    match v {
        Value::Inst(inst) => {
            inst.tdef.name.as_ref() == name || inst.tdef.interfaces.iter().any(|s| s.as_ref() == name)
        }
        Value::Map(m) => error_map_chain(m).is_some_and(|chain| chain.contains(&name)),
        _ => false,
    }
}

fn pred_illegal_argument_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.IllegalArgumentException")
}
fn pred_index_out_of_bounds_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.IndexOutOfBoundsException")
}
fn pred_runtime_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.RuntimeException")
}
// mova/PLAN.md interop-census batch: `IllegalStateException`/
// `UnsupportedOperationException` ctors, same RuntimeException-rooted
// veneer as the rows above.
fn pred_illegal_state_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.IllegalStateException")
}
fn pred_unsupported_operation_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.UnsupportedOperationException")
}
fn pred_arithmetic_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.ArithmeticException")
}
fn pred_class_cast_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.ClassCastException")
}
fn pred_null_pointer_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.NullPointerException")
}
fn pred_number_format_exception(v: &Value) -> bool {
    is_exception_named(v, "java.lang.NumberFormatException")
}
fn pred_exception_class(v: &Value) -> bool {
    is_exception_named(v, "java.lang.Exception")
}
fn pred_throwable(v: &Value) -> bool {
    is_exception_named(v, "java.lang.Throwable")
}
fn pred_assertion_error(v: &Value) -> bool {
    is_exception_named(v, "java.lang.AssertionError")
}
fn pred_stack_overflow_error(v: &Value) -> bool {
    is_exception_named(v, "java.lang.StackOverflowError")
}
fn pred_error(v: &Value) -> bool {
    is_exception_named(v, "java.lang.Error")
}
// W4-veneer (try_catch.clj's `catch-receives-checked-exception-from-eval`):
// `java.io.FileNotFoundException` -- see `mk_file_not_found`'s doc.
fn pred_file_not_found_exception(v: &Value) -> bool {
    is_exception_named(v, "java.io.FileNotFoundException")
}
// C3c: `clojure.lang.ArityException` -- see `mk_arity_exception`'s doc.
fn pred_arity_exception(v: &Value) -> bool {
    is_exception_named(v, "clojure.lang.ArityException")
}
// W4-EVAL task 4: `java.lang.IllegalAccessError` -- see
// `ILLEGAL_ACCESS_ERROR_ANCESTORS`'s doc.
fn pred_illegal_access_error(v: &Value) -> bool {
    is_exception_named(v, "java.lang.IllegalAccessError")
}

/// S6: `java.net.URI` class-membership predicate, registered alongside
/// the exception classes below (same "bind a `ClassVal::Builtin` global
/// var" shape) -- true only for our narrow `Value::Uri` (see that
/// variant's own doc for scope).
fn pred_uri(v: &Value) -> bool {
    matches!(v, Value::Uri(_))
}

// ---------------------------------------------------------------------
// S6: boxed-numeric/Boolean/Character/BigDecimal/BigInteger constructors.
//
// COMPAT VENEER, NOT JVM EMULATION: mova's real type system is
// Rust-native (`Value::Int`/`Value::Float`/etc., collapsed per
// CLOJURE-COMPAT-PLAN.md's scope decision -- there is no boxed
// `Integer`/`Float`/... `Value` variant and none is planned). These ten
// `(Long. x)`/`(Float. x)`/... arms exist ONLY because
// `tests/clojure-suite/vendor/numbers.clj` spells Java's boxed-number
// idiom directly (`(Byte. Byte/MAX_VALUE)` etc.) -- they translate that
// ONE vendored-suite idiom onto mova's EXISTING numeric/bool/char
// values, not the start of a `java.lang` object model. `(Long. 3)`
// returns the exact same `Value::Int(3)` `3` itself already is; `(class
// (Long. 3))` and `(class 3)` are consequently IDENTICAL (both
// `java.lang.Long`) -- this is not a special case, it falls out of
// there being only one `Value::Int`. Every measured mismatch this
// causes (`(class (Float. 1.5))` should be `java.lang.Float`, not
// `java.lang.Double`) is a documented, permanent `DEVIATIONS.md` entry,
// not a bug to chase -- see `tests/conformance/DEVIATIONS.md`'s S6
// section. Go no deeper than the vendored suite measurably demands: do
// not add ctor overloads, ranges, or JVM exception classes this module
// doesn't already have oracle evidence for.
//
// Every arg-acceptance rule below was measured against real Clojure
// 1.13.0-alpha6 (`clojure -M`, this task's own scratchpad probe) rather
// than guessed from the JDK's javadoc: real `new` is a Clojure-COMPILER
// reflective ctor match, not full Java overload resolution -- `(Integer.
// 3)` succeeds (an int-range literal compiles as a constant `int`) but
// `(Double. 3)`/`(Short. 3)`/`(Byte. 3)` all throw "No matching ctor
// found", because `Double`/`Short`/`Byte` have no int-taking
// constructor and Clojure's own ctor matcher does not widen a `long`
// argument the way `javac`'s overload resolution would. mova cannot
// (and, per the compat-veneer framing above, should not) reproduce that
// literal-vs-runtime-value compile-time distinction -- it has exactly
// one `Value::Int`, so a numeric arg to an integral ctor is accepted
// UNCONDITIONALLY (subject to the target's range, checked in
// `int_ctor`) whether or not it "looks like" an int-range literal on
// the JVM. That is deliberately MORE permissive than the JVM for e.g.
// `(Short. 3)` (JVM throws, mova succeeds) -- an accepted, unavoidable
// consequence of the collapsed numeric model, not independently
// tracked, since the vendored suite only ever passes already-narrow
// values (`Short/MAX_VALUE` etc.) here.

/// Shared arity gate for every boxed ctor: real Clojure has no `Long`/
/// `Float`/... constructor overload of any arity but exactly one
/// (JVM boxed-number/Boolean/Character/BigDecimal/BigInteger classes all
/// reject 0 args and >1 args at the `new` reflective-match step).
fn boxed_ctor(
    args: &[Value],
    span: Span,
    class: &str,
    f: impl FnOnce(&Value) -> Result<Value, RjError>,
) -> Result<Value, RjError> {
    match args {
        [v] => f(v).map_err(|e| e.with_span(span)),
        _ => Err(RjError::arity(format!("{class}: expected exactly 1 arg, got {}", args.len())).with_span(span)),
    }
}

/// `Long`/`Integer`/`Short`/`Byte` ctor body: a `Value::Int` already
/// within `[lo, hi]` (mova's stand-in for "already the right JVM
/// primitive width" -- see the module-doc note on why this is more
/// permissive than the JVM for `Short`/`Byte`), or a numeric string
/// parsed the same way `Long/parseLong`/`Integer/valueOf`
/// (`src/builtins/statics.rs`) already do -- measured: `(Integer.
/// "x")` throws `NumberFormatException` on the JVM, matched here by an
/// ordinary `RjError`. A `Value::Float` arg is rejected (measured:
/// `(Long. 1.5)` => "No matching ctor found").
fn int_ctor(v: &Value, class: &str, lo: i64, hi: i64) -> Result<Value, RjError> {
    match v {
        Value::Int(n) if *n >= lo && *n <= hi => Ok(Value::Int(*n)),
        Value::Int(n) => Err(RjError::type_err(format!("{class}: {n} is out of range"))),
        Value::Str(s) => {
            let n: i64 = s.trim().parse().map_err(|_| RjError::type_err(format!("{class}: invalid input {s:?}")))?;
            if n < lo || n > hi {
                return Err(RjError::type_err(format!("{class}: {n} is out of range")));
            }
            Ok(Value::Int(n))
        }
        other => Err(RjError::type_err(format!("{class}: expected an integer or string, got {}", other.type_name()))),
    }
}

/// `Double`/`Float` ctor body: a `Value::Float` (mova's one floating
/// type stands in for both JVM widths -- see module doc), or a numeric
/// string. `narrow_f32` mirrors `Float/parseFloat`
/// (`src/builtins/statics.rs`): parse as a REAL `f32` first, then widen,
/// so `(Float. "3.14")` stores the numerically-correct float-rounded
/// value rather than a bare `f64` parse. A `Value::Int` arg is rejected
/// (measured: `(Double. 3)` => "No matching ctor found" -- unlike
/// `Integer`, `Double`/`Float` have no int-taking constructor at all).
fn float_ctor(v: &Value, class: &str, narrow_f32: bool) -> Result<Value, RjError> {
    match v {
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::Str(s) => {
            let s = s.trim();
            if narrow_f32 {
                s.parse::<f32>().map(|f| Value::Float(f as f64))
            } else {
                s.parse::<f64>().map(Value::Float)
            }
            .map_err(|_| RjError::type_err(format!("{class}: invalid input {s:?}")))
        }
        other => Err(RjError::type_err(format!("{class}: expected a floating-point number or string, got {}", other.type_name()))),
    }
}

/// `Boolean` ctor body: `Value::Bool` passes through; a string is
/// case-insensitively compared to `"true"` (measured: `(Boolean.
/// "TrUe")` => `true`, `(Boolean. "nope")` => `false` -- `Boolean(String)`
/// NEVER throws, unlike every numeric ctor's string overload). Any other
/// arg type (measured: `(Boolean. 1)`) throws "No matching ctor found".
fn boolean_ctor(v: &Value) -> Result<Value, RjError> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(*b)),
        Value::Str(s) => Ok(Value::Bool(s.eq_ignore_ascii_case("true"))),
        other => Err(RjError::type_err(format!("Boolean: expected a boolean or string, got {}", other.type_name()))),
    }
}

/// `Character` ctor body: `Character(char)` is the ONLY constructor real
/// Clojure's `new` can match here (measured: `(Character. 3)` throws --
/// an int does not implicitly become a `char`), so this accepts
/// `Value::Char` only.
fn character_ctor(v: &Value) -> Result<Value, RjError> {
    match v {
        Value::Char(c) => Ok(Value::Char(*c)),
        other => Err(RjError::type_err(format!("Character: expected a char, got {}", other.type_name()))),
    }
}

/// `BigDecimal` ctor body. `Int`/`Str` go through the exact same
/// representations `bigint`/`bigdec`-family code already builds
/// (`BigDecVal::from_i64`/`BigDecVal::parse`, `src/bignum.rs`). `Float`
/// is the one case that is NOT `bigdec`'s shortest-round-tripping
/// behavior: real `new BigDecimal(double)` takes the EXACT binary value
/// of the `double` (measured: `(str (BigDecimal. 0.1))` =>
/// `"0.1000000000000000055511151231257827021181583404541015625"`, a
/// 55-scale expansion, wildly different from `(bigdec 0.1)` =>
/// `0.1M`) -- `exact_bigdec_from_f64` below reproduces that bit-exact
/// algorithm rather than deviating, since it is a self-contained,
/// measurable piece of arithmetic, not an architecture gap.
fn bigdecimal_ctor(v: &Value) -> Result<Value, RjError> {
    match v {
        Value::Int(n) => Ok(Value::BigDec(Arc::new(BigDecVal::from_i64(*n)))),
        Value::Float(f) => Ok(Value::BigDec(Arc::new(exact_bigdec_from_f64(*f)?))),
        Value::Str(s) => BigDecVal::parse(s)
            .map(|d| Value::BigDec(Arc::new(d)))
            .ok_or_else(|| RjError::type_err(format!("BigDecimal: invalid input {s:?}"))),
        other => Err(RjError::type_err(format!("BigDecimal: expected a number or string, got {}", other.type_name()))),
    }
}

/// `new BigDecimal(double)`'s exact-binary-value algorithm (measured
/// against `.oracle`, see `bigdecimal_ctor`'s doc): a finite IEEE-754
/// `double` is `sign * mantissa * 2^exp2` for an INTEGER `mantissa`
/// (53-bit, implicit leading 1 restored for normals) and integer `exp2`;
/// `unscaled`/`scale` follow by rewriting `2^exp2` as `5^-exp2 /
/// 10^-exp2` when `exp2 < 0` (so `BigDecimal`'s base-10 `(unscaled,
/// scale)` stays exact, never a binary approximation) or as a plain
/// left-shift when `exp2 >= 0`. Zero is special-cased (measured: `(str
/// (BigDecimal. 0.0))` => `"0"`, scale `0` -- the bit-decomposition
/// branch would otherwise compute a large positive `scale` for the
/// subnormal-zero exponent, which is NOT what real `BigDecimal(0.0)`
/// does, since the JVM's own constructor special-cases zero before ever
/// reaching the bit math). This is `java.math.BigDecimal`'s documented
/// constructor algorithm, not a mova invention -- ported here rather
/// than reused because mova has no JVM to call it on.
fn exact_bigdec_from_f64(f: f64) -> Result<BigDecVal, RjError> {
    if !f.is_finite() {
        return Err(RjError::type_err(format!(
            "BigDecimal: {} has no exact decimal representation",
            crate::printer::display_str(&Value::Float(f))
        )));
    }
    if f == 0.0 {
        return Ok(BigDecVal::new(BigInt::from(0), 0));
    }
    let bits = f.to_bits();
    let negative = bits >> 63 == 1;
    let raw_exp = ((bits >> 52) & 0x7FF) as i64;
    let raw_mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    let (mut mantissa, mut exp2) = if raw_exp == 0 {
        // Subnormal: no implicit leading 1, fixed exponent bias floor.
        (raw_mantissa, -1074i64)
    } else {
        (raw_mantissa | (1u64 << 52), raw_exp - 1075)
    };
    // Normalize: strip trailing zero BITS from `mantissa` into `exp2`
    // (`f != 0.0` here, so `mantissa != 0` and this terminates). This is
    // `java.math.BigDecimal`'s OWN normalization step, not an
    // optimization -- measured: skipping it computes a numerically
    // identical but WRONGLY-SCALED value (one extra trailing zero digit
    // for `0.1`, scale `56` instead of the oracle's `55`), because an
    // even mantissa means `2^exp2`'s sign-flipped `5^-exp2/10^-exp2`
    // rewrite (below) isn't yet in LOWEST terms.
    while mantissa & 1 == 0 {
        mantissa >>= 1;
        exp2 += 1;
    }
    let mantissa_big = BigInt::from(mantissa);
    let (unscaled, scale) = if exp2 >= 0 {
        (mantissa_big * BigInt::from(2).pow(exp2 as u32), 0i32)
    } else {
        let shift = (-exp2) as u32;
        (mantissa_big * BigInt::from(5).pow(shift), shift as i32)
    };
    let unscaled = if negative { -unscaled } else { unscaled };
    Ok(BigDecVal::new(unscaled, scale))
}

/// `BigInteger` ctor body: `BigInteger(String)` is the ONLY constructor
/// this task's oracle measurement found reachable from a plain numeric
/// literal (measured: `(BigInteger. 3)` throws "No matching ctor found"
/// -- `java.math.BigInteger` has no `int`/`long`-taking constructor at
/// all, unlike every other boxed-number class here). String parsing
/// reuses the same `num_bigint::BigInt` grammar `bigint`/`biginteger`
/// (`src/builtins/numbers.rs`) already parse against.
fn biginteger_ctor(v: &Value) -> Result<Value, RjError> {
    match v {
        Value::Str(s) => s
            .parse::<BigInt>()
            .map(|n| Value::BigInteger(Arc::new(BigIntVal(n))))
            .map_err(|_| RjError::type_err(format!("BigInteger: invalid input {s:?}"))),
        other => Err(RjError::type_err(format!("BigInteger: expected a string, got {}", other.type_name()))),
    }
}

// ---------------------------------------------------------------------
// Method dispatch: `(.method target args...)` when `target` is a
// `Value::HostInst`. Called from `eval::types_forms::eval_dot_form`.
// ---------------------------------------------------------------------

fn unknown_method(kind: HostKind, method: &str, argc: usize, span: Span) -> RjError {
    RjError::other(format!(
        "no method .{method} (with {argc} arg(s)) on {}",
        kind.diagnostic_name()
    ))
    .with_span(span)
}

pub fn call_method(
    interp: &mut Interp,
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match h.kind {
        HostKind::Random => call_random_method(h, method, args, span),
        HostKind::Date => call_date_method(h, method, args, span),
        HostKind::Thread => call_thread_method(interp, h, method, args, span),
        HostKind::ThreadLocal => call_threadlocal_method(interp, h, method, args, span),
        HostKind::CyclicBarrier => call_barrier_method(h, method, args, span),
        // clojure-lsp campaign (mova/PLAN.md): `StringBuilder`/
        // `StringBuffer` are the ONE `java.*` veneer this campaign owns
        // (see mova/PLAN.md's "java.* veneer" rule) -- `rewrite-clj.
        // reader`'s `read-char`-level token buffer (`(.append ^String
        // Builder buf (char c))`, then `(.toString buf)`) builds every
        // token mova's reader path reads, so a construction-only veneer
        // (real Clojure semantics: mutable, not persistent) cannot carry
        // clojure-lsp's own `clojure-lsp.parser` at all. See
        // `call_charbuf_method`'s doc for the exact method surface.
        HostKind::StringBuilder | HostKind::StringBuffer => {
            call_charbuf_method(h, method, args, span)
        }
        // Construction-only (see `HostKind`'s doc) -- every method call
        // is the same "not supported" shape every other unimplemented
        // host method gets.
        HostKind::ArrayList
        | HostKind::HashSet
        | HostKind::Object
        | HostKind::Locale => Err(unknown_method(h.kind, method, args.len(), span)),
        HostKind::HashMap => call_hashmap_method(h, method, args, span),
        HostKind::Iterator => call_iterator_method(interp, h, method, args, span),
        HostKind::Spliterator => call_spliterator_method(interp, h, method, args, span),
        HostKind::Stream => call_stream_method(h, method, args, span),
        // A marker with no methods of its own (see `HostKind::Collector`)
        // -- it is only ever an ARGUMENT, to `.collect`.
        HostKind::Collector => Err(unknown_method(h.kind, method, args.len(), span)),
        HostKind::JavaFile => call_javafile_method(h, method, args, span),
        HostKind::BufferedReader => call_bufferedreader_method(h, method, args, span),
        HostKind::StringReader => call_string_reader_method(h, method, args, span),
        HostKind::ReflectorFixture => call_reflector_fixture_method(h, method, args, span),
        HostKind::ReentrantLock => call_reentrant_lock_method(h, method, args, span),
        HostKind::InputStream => call_input_stream_method(h, method, args, span),
        HostKind::OutputStream => call_output_stream_method(h, method, args, span),
        HostKind::Clock => call_clock_method(h, method, args, span),
        HostKind::Instant => call_instant_method(h, method, args, span),
        HostKind::StringTokenizer => call_string_tokenizer_method(h, method, args, span),
        HostKind::MessageDigest => call_message_digest_method(h, method, args, span),
        HostKind::JarFile => call_jar_file_method(h, method, args, span),
        HostKind::JarEntry => call_jar_entry_method(h, method, args, span),
        HostKind::JarEntries => call_jar_entries_method(h, method, args, span),
        HostKind::RandomAccessFile | HostKind::FileChannel | HostKind::FileLock => {
            call_file_lock_method(h, method, args, span)
        }
        HostKind::Url | HostKind::HttpConnection => call_http_method(h, method, args, span),
    }
}

/// kondo-wave: `.lock`/`.unlock`/`.tryLock`/`.isLocked` on a
/// `(ReentrantLock.)` -- a REAL reentrant mutex. `.lock` blocks (via the
/// inner `Condvar`) until either nobody owns it or THIS thread already
/// does (reentrant: bumps `count`); `.unlock` decrements `count` and
/// only releases (clears `owner`, wakes waiters) at zero. `.unlock`
/// called by a non-owner mirrors real Java's `IllegalMonitorStateException`.
fn call_reentrant_lock_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    let pair = {
        let HostState::ReentrantLock(pair) = &*crate::sync::lock_mutex(&h.state) else {
            unreachable!("HostKind::ReentrantLock always carries HostState::ReentrantLock")
        };
        pair.clone()
    };
    let (inner, cv) = &*pair;
    let this_thread = std::thread::current().id();
    match (method, args) {
        ("lock", []) => {
            let mut st = crate::sync::lock_mutex(inner);
            loop {
                match st.owner {
                    Some(id) if id == this_thread => {
                        st.count += 1;
                        break;
                    }
                    None => {
                        st.owner = Some(this_thread);
                        st.count = 1;
                        break;
                    }
                    Some(_) => st = crate::sync::cv_wait(cv, st),
                }
            }
            Ok(Value::Nil)
        }
        ("unlock", []) => {
            let mut st = crate::sync::lock_mutex(inner);
            if st.owner != Some(this_thread) {
                return Err(RjError::other(
                    "java.lang.IllegalMonitorStateException: unlock by a thread that does not hold the lock",
                ));
            }
            st.count -= 1;
            if st.count == 0 {
                st.owner = None;
                cv.notify_all();
            }
            Ok(Value::Nil)
        }
        ("tryLock", []) => {
            let mut st = crate::sync::lock_mutex(inner);
            match st.owner {
                Some(id) if id == this_thread => {
                    st.count += 1;
                    Ok(Value::Bool(true))
                }
                None => {
                    st.owner = Some(this_thread);
                    st.count = 1;
                    Ok(Value::Bool(true))
                }
                Some(_) => Ok(Value::Bool(false)),
            }
        }
        ("isLocked", []) => {
            let st = crate::sync::lock_mutex(inner);
            Ok(Value::Bool(st.owner.is_some()))
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// Reads the path string out of a `HostKind::JavaFile` cell -- shared by
/// every `call_javafile_method` arm below and by `mk_file_writer`'s
/// `io/writer`-on-a-File call path.
fn javafile_path(h: &Arc<HostInstVal>) -> Str {
    let HostState::JavaFile(p) = &*crate::sync::lock_mutex(&h.state) else {
        unreachable!("HostKind::JavaFile always carries HostState::JavaFile")
    };
    p.clone()
}

/// lsp/io: the `clojure.java.io` shim's uniform "accept a `File` or a bare
/// path string" convenience (real `io/as-file`'s job on the JVM) --
/// `pub(crate)` so `builtins::io`'s `copy`/`make-parents`/`file-seq`
/// natives can extract a path without knowing `HostState`'s private
/// shape.
pub(crate) fn path_str_of(v: &Value) -> Option<Str> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::HostInst(h) if h.kind == HostKind::JavaFile => Some(javafile_path(h)),
        _ => None,
    }
}

/// W4-veneer: `.toPath` on a `java.io.File` -- see `HostKind::JavaFile`'s
/// doc for why this hands back the identical `HostInst` rather than
/// minting a distinct `Path` value.
///
/// kondo-wave: added the `isFile`/`isDirectory`/`exists`/`getName`/
/// `getPath`/`getParent(File)`/`getAbsolute(Path|File)`/
/// `getCanonical(Path|File)`/`mkdir(s)`/`delete`/`list(Files)`/
/// `isAbsolute` methods clj-kondo's `impl/core.clj` (directory walking,
/// `.clj-kondo` config-dir discovery) and the clojure-lsp-kondo overlay's
/// `clojure.java.io`/`babashka.fs` shims call on a `File`/`Path`. Each
/// arm is a one-line call into `builtins::fileio` (the actual path-string
/// logic, see that module's doc) -- kept that way so this shared-file
/// match stays a plain dispatch table other waves editing OTHER `HostKind`
/// arms in this file won't collide with.
fn call_javafile_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    use crate::builtins::fileio;
    let path = {
        let HostState::JavaFile(p) = &*crate::sync::lock_mutex(&h.state) else {
            unreachable!("HostKind::JavaFile always carries HostState::JavaFile")
        };
        p.clone()
    };
    let p = path.as_ref();
    match (method, args) {
        ("toPath", []) => Ok(Value::HostInst(h.clone())),
        ("exists", []) => {
            let p = javafile_path(h);
            Ok(Value::Bool(std::path::Path::new(p.as_ref()).exists()))
        }
        ("isDirectory", []) => {
            let p = javafile_path(h);
            Ok(Value::Bool(std::path::Path::new(p.as_ref()).is_dir()))
        }
        ("isFile", []) => {
            let p = javafile_path(h);
            Ok(Value::Bool(std::path::Path::new(p.as_ref()).is_file()))
        }
        ("canRead", []) => {
            let p = javafile_path(h);
            Ok(Value::Bool(std::fs::metadata(p.as_ref()).is_ok()))
        }
        // `.length` -- real `File.length()` returns the file's size in
        // bytes as a `long`, or `0L` if the file does not exist (it never
        // throws for a missing file). Needed by clj-kondo's own
        // single-file lint path (`kondo/run!` on a `:lint [file]` config
        // sizes a buffer via `.length` up front); without this arm every
        // incremental `textDocument/didOpen` re-lint (which lints a real
        // temp file rather than re-scanning the whole project) threw "no
        // method .length" inside the kondo future, and clojure-lsp's own
        // try/catch there silently swallowed it -- dropping that file's
        // publishDiagnostics entirely, including the "no diagnostics"
        // empty-array notification the JVM always sends.
        ("length", []) => {
            let p = javafile_path(h);
            let len = std::fs::metadata(p.as_ref()).map(|m| m.len()).unwrap_or(0);
            Ok(Value::Int(len as i64))
        }
        // kondo-wave addition: `.isAbsolute` -- clj-kondo's config-dir
        // discovery checks this on a `File`.
        ("isAbsolute", []) => Ok(Value::Bool(fileio::is_absolute(p))),
        ("getName", []) => {
            let p = javafile_path(h);
            let name = std::path::Path::new(p.as_ref())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok(Value::Str(Str::from(name)))
        }
        ("getPath", []) => Ok(Value::Str(javafile_path(h))),
        // kondo-wave addition: `.getParent` (String-returning sibling of
        // `.getParentFile` below).
        ("getParent", []) => {
            Ok(fileio::parent(p).map(|s| Value::Str(s.into())).unwrap_or(Value::Nil))
        }
        ("getAbsolutePath", []) => {
            let p = javafile_path(h);
            let path = std::path::Path::new(p.as_ref());
            let abs = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            Ok(Value::Str(Str::from(abs.to_string_lossy().into_owned())))
        }
        // kondo-wave addition: `.getAbsoluteFile` (File-returning sibling).
        ("getAbsoluteFile", []) => Ok(mk_java_file(fileio::absolute_path(p).into())),
        ("getCanonicalPath", []) => {
            let p = javafile_path(h);
            match std::fs::canonicalize(p.as_ref()) {
                Ok(canon) => Ok(Value::Str(Str::from(canon.to_string_lossy().into_owned()))),
                // Real `getCanonicalPath` throws `IOException` when the
                // path can't be resolved; nothing in scope calls it on a
                // nonexistent path, so this narrower "fall back to the
                // absolute path" is the honest thing to do without
                // inventing an exception type no caller catches.
                Err(_) => call_javafile_method(h, "getAbsolutePath", args, span),
            }
        }
        // kondo-wave addition: `.getCanonicalFile` (File-returning sibling).
        ("getCanonicalFile", []) => Ok(mk_java_file(fileio::canonical_path(p).into())),
        ("getParentFile", []) => {
            let p = javafile_path(h);
            match std::path::Path::new(p.as_ref()).parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    Ok(mk_java_file(Str::from(parent.to_string_lossy().into_owned())))
                }
                _ => Ok(Value::Nil),
            }
        }
        ("mkdirs", []) => {
            let p = javafile_path(h);
            Ok(Value::Bool(std::fs::create_dir_all(p.as_ref()).is_ok()))
        }
        // kondo-wave addition: `.mkdir` (single-level, sibling of `.mkdirs`).
        ("mkdir", []) => Ok(Value::Bool(fileio::mkdir(p))),
        // kondo-wave addition: `.delete`.
        ("delete", []) => Ok(Value::Bool(fileio::delete(p))),
        // kondo-wave addition: `.createNewFile` (clj-kondo's cache lock file).
        ("createNewFile", []) => Ok(Value::Bool(fileio::create_new_file(p))),
        // kondo-wave addition: `.list` (name-only sibling of `.listFiles`).
        ("list", []) => Ok(fileio::list_names(p)
            .map(|names| Value::Vector(names.into_iter().map(|n| Value::Str(n.into())).collect()))
            .unwrap_or(Value::Nil)),
        ("listFiles", []) => {
            let p = javafile_path(h);
            match std::fs::read_dir(p.as_ref()) {
                Ok(entries) => {
                    let files: PVec = entries
                        .filter_map(|e| e.ok())
                        .map(|e| mk_java_file(Str::from(e.path().to_string_lossy().into_owned())))
                        .collect();
                    Ok(Value::Vector(files))
                }
                Err(_) => Ok(Value::Nil),
            }
        }
        ("lastModified", []) => {
            let p = javafile_path(h);
            let millis = std::fs::metadata(p.as_ref())
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(Value::Int(millis))
        }
        // kondo-wave: `File.toURI()` returns `Value::Uri` (kept over the
        // integration side's plain `Value::Str`) -- `Value::Uri` is
        // already a wrapped path string (see `builtins::statics`'s
        // `URI/create` and `printer.rs`'s `Value::Uri` arm), needed so
        // `(instance? java.net.URI (.toURI file))` resolves.
        ("toURI", []) => {
            let abs = fileio::absolute_path(p);
            let with_slash = if fileio::is_directory(p) && !abs.ends_with('/') {
                format!("{abs}/")
            } else {
                abs
            };
            Ok(Value::Uri(format!("file:{with_slash}").into()))
        }
        // lsp/io: `.toFile` on a `Path` -- since `HostKind::JavaFile`
        // already stands in for `Path` (see `.toPath`'s doc above),
        // `.toFile` is just identity, same shape as `.toPath` itself.
        ("toFile", []) => Ok(Value::HostInst(h.clone())),
        // lsp/io: `.normalize` (`java.nio.file.Path`) -- purely lexical
        // "."/".." collapsing, no filesystem access (unlike
        // `.getCanonicalPath`, which resolves symlinks and needs the
        // path to exist). Keeps a leading "/" (absolute) if present.
        ("normalize", []) => {
            let s = javafile_path(h);
            let absolute = s.starts_with('/');
            let mut out: Vec<&str> = Vec::new();
            for seg in s.split('/') {
                match seg {
                    "" | "." => {}
                    ".." => {
                        if matches!(out.last(), Some(last) if *last != "..") {
                            out.pop();
                        } else if !absolute {
                            out.push("..");
                        }
                    }
                    _ => out.push(seg),
                }
            }
            let joined = out.join("/");
            let normalized = if absolute { format!("/{joined}") } else { joined };
            Ok(mk_java_file(Str::from(normalized)))
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

// ---------------------------------------------------------------------
// lsp/io (review round 2): real streams -- `HostKind::InputStream`/
// `OutputStream`. `mk_input_stream`/`mk_output_stream` are the ONE place
// that wraps a source/sink in the buffered box; every native in
// `builtins::io` (`mova.io/file-input-stream`, `System/in`, ...) and
// every dot-method below goes through these plus the `stream_*` helpers,
// so the actual read/write logic exists exactly once regardless of
// whether it's reached through `.read`/`.write` (Clojure dot-interop) or
// `mova.io/read-bytes`/`write-str` (the jsonrpc4clj overlay's calls).
// ---------------------------------------------------------------------

/// Wraps any `Read + Send` source (real stdin, an opened file, an
/// in-memory `Cursor`) as a `HostKind::InputStream`. Buffered
/// (`BufReader`) so a byte-at-a-time header-line read
/// (`stream_read_line`) doesn't cost one syscall per byte.
pub(crate) fn mk_input_stream(r: Box<dyn std::io::Read + Send>) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::InputStream,
        state: Mutex::new(HostState::InputStream(BoxedReader(std::io::BufReader::new(r)))),
    }))
}

/// Wraps any `Write + Send` sink (real stdout/stderr, an opened file) as
/// a `HostKind::OutputStream`. Buffered (`BufWriter`); `.write`/`.append`
/// stream straight through it -- see `HostState::OutputStream`'s doc for
/// why this replaced the old buffer-then-write-whole-file `FileWriter`
/// design.
pub(crate) fn mk_output_stream(w: Box<dyn std::io::Write + Send>) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::OutputStream,
        state: Mutex::new(HostState::OutputStream(BoxedWriter(std::io::BufWriter::new(w)))),
    }))
}

/// `(java.time.Clock/systemDefaultZone)` -- see `HostKind::Clock`'s doc.
pub(crate) fn mk_clock() -> Value {
    Value::HostInst(Arc::new(HostInstVal { kind: HostKind::Clock, state: Mutex::new(HostState::Clock) }))
}

/// `(java.security.MessageDigest/getInstance algorithm)` -- see
/// `HostKind::MessageDigest`'s doc. Only `"MD5"`/`"SHA-256"` are
/// supported (the only two algorithms this campaign's corpus asks for);
/// any other name mirrors real `MessageDigest.getInstance`'s checked
/// `NoSuchAlgorithmException` shape closely enough for this scope (a
/// plain type error -- no real checked-exception class hierarchy on
/// mova, see `SHIM-LIMITS.md`).
pub(crate) fn mk_message_digest(algorithm: &Str) -> Result<Value, RjError> {
    match algorithm.as_ref() {
        "MD5" | "SHA-256" => Ok(Value::HostInst(Arc::new(HostInstVal {
            kind: HostKind::MessageDigest,
            state: Mutex::new(HostState::MessageDigest(algorithm.clone())),
        }))),
        other => Err(RjError::type_err(format!(
            "MessageDigest/getInstance: unsupported algorithm {other} (mova supports MD5, SHA-256)"
        ))),
    }
}

/// `.digest(byte[])` -- the sole method the corpus calls on a
/// `MessageDigest` (see `HostState::MessageDigest`'s doc: no `.update`
/// call site exists, so this is always one-shot). Returns a real byte
/// array (`ArrayKind::Byte`, signed, same convention as `.getBytes` in
/// `builtins::strings`), matching `MessageDigest.digest(byte[])`'s
/// return type exactly.
fn call_message_digest_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    use md5::{Digest, Md5};
    use sha2::Sha256;
    match (method, args) {
        ("digest", [Value::Array(arr)]) if arr.kind == crate::value::ArrayKind::Byte => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::MessageDigest(algorithm) = &*guard else {
                unreachable!("HostKind::MessageDigest state mismatch")
            };
            let bytes: Vec<u8> = crate::sync::lock_mutex(&arr.data)
                .iter()
                .map(|v| match v {
                    Value::Int(n) => *n as u8,
                    _ => 0,
                })
                .collect();
            let out: Vec<u8> = match algorithm.as_ref() {
                "MD5" => Md5::digest(&bytes).to_vec(),
                "SHA-256" => Sha256::digest(&bytes).to_vec(),
                _ => unreachable!("mk_message_digest only constructs MD5/SHA-256"),
            };
            Ok(Value::Array(Arc::new(ArrayVal {
                kind: crate::value::ArrayKind::Byte,
                dims: 1,
                data: Mutex::new(out.into_iter().map(|b| Value::Int(b as i8 as i64)).collect()),
            })))
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

// ---------------------------------------------------------------------
// lsp/kondo: `java.util.jar.JarFile`/`java.util.zip.ZipFile` veneer --
// the exact surface clj-kondo's `core.clj`/`analysis/java.clj` and
// clojure-lsp's `config.clj`/`java_interop.clj`/`diagnostics/custom.clj`
// use: ctor from path/File, `.entries` (+ `enumeration-seq`),
// `.getEntry`/`.getJarEntry`, entry `.getName`/`.isDirectory`/
// `.getSize`, `.getInputStream`, `.close`. Reads the zip central
// directory eagerly (cheap metadata scan, no decompression -- `zip`
// crate's `by_index_raw`) but inflates an entry's bytes only when
// `.getInputStream` is actually called on it (`mova/PLAN.md` "open
// lazily; read entries on demand").
// ---------------------------------------------------------------------

/// e2: ancestors of `java.io.IOException` subclasses thrown by the URL/HTTP veneer.
const IO_EXCEPTION_ANCESTORS: &[&str] = &["java.io.IOException", "java.lang.Exception", "java.lang.Throwable"];

/// e2: an `IOException` value carrying `msg`, catchable as `IOException`/`Exception`.
pub(crate) fn mk_io_exception(msg: String) -> Value {
    mk_exception("java.io.IOException", &["java.lang.Exception", "java.lang.Throwable"], Some(Value::Str(Str::from(msg))), None)
}

/// e2: the `java.net.URL` spelling of `v`, if it is one.
pub(crate) fn url_str_of(v: &Value) -> Option<Str> {
    match v {
        Value::HostInst(h) if h.kind == HostKind::Url => match &*crate::sync::lock_mutex(&h.state) {
            HostState::Url(u) => Some(u.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// e2: GET `url` into a fresh `InputStream` (`.openStream`, `slurp`/`io/input-stream` of a URL).
pub(crate) fn http_open_stream(url: &str) -> Result<Value, RjError> {
    let r = crate::http::get(url, None, None).map_err(|e| RjError::thrown(mk_io_exception(e)))?;
    if r.status >= 400 {
        return Err(RjError::thrown(mk_io_exception(format!(
            "Server returned HTTP response code: {} for URL: {url}",
            r.status
        ))));
    }
    Ok(mk_input_stream(r.body))
}

fn call_http_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    if h.kind == HostKind::Url {
        let url = url_str_of(&Value::HostInst(h.clone())).unwrap_or_default();
        return match (method, args) {
            ("openConnection", []) => Ok(Value::HostInst(Arc::new(HostInstVal {
                kind: HostKind::HttpConnection,
                state: Mutex::new(HostState::HttpConn { url, connect_ms: None, read_ms: None, resp: None }),
            }))),
            ("openStream", []) => http_open_stream(&url).map_err(|e| e.with_span(span)),
            ("toString" | "toExternalForm", []) => Ok(Value::Str(url)),
            _ => Err(RjError::other(format!("no method {method} with {} args on java.net.URL", args.len())).with_span(span)),
        };
    }
    let mut g = crate::sync::lock_mutex(&h.state);
    let HostState::HttpConn { url, connect_ms, read_ms, resp } = &mut *g else { unreachable!() };
    let ms_arg = |a: &[Value]| match a {
        [Value::Int(n)] => Ok(Some((*n).max(0) as u64)),
        _ => Err(RjError::type_err(format!("{method}: expected an int")).with_span(span)),
    };
    match (method, args) {
        ("setConnectTimeout", [_]) => {
            *connect_ms = ms_arg(args)?;
            return Ok(Value::Nil);
        }
        ("setReadTimeout", [_]) => {
            *read_ms = ms_arg(args)?;
            return Ok(Value::Nil);
        }
        ("disconnect", []) => {
            *resp = None;
            return Ok(Value::Nil);
        }
        ("getURL", []) => {
            return Ok(Value::HostInst(Arc::new(HostInstVal {
                kind: HostKind::Url,
                state: Mutex::new(HostState::Url(url.clone())),
            })))
        }
        ("connect" | "getResponseCode" | "getInputStream" | "getContentType", []) => {}
        _ => {
            return Err(RjError::other(format!(
                "no method {method} with {} args on javax.net.ssl.HttpsURLConnection",
                args.len()
            ))
            .with_span(span))
        }
    }
    if resp.is_none() {
        let r = crate::http::get(url, *connect_ms, *read_ms)
            .map_err(|e| RjError::thrown(mk_io_exception(e)).with_span(span))?;
        *resp = Some(HttpResp { status: r.status, content_type: r.content_type, body: Some(r.body) });
    }
    let r = resp.as_mut().expect("filled above");
    match method {
        "getResponseCode" => Ok(Value::Int(r.status as i64)),
        "getContentType" => Ok(r.content_type.clone().map(|c| Value::Str(Str::from(c))).unwrap_or(Value::Nil)),
        "getInputStream" => {
            if r.status >= 400 {
                return Err(RjError::thrown(mk_io_exception(format!(
                    "Server returned HTTP response code: {} for URL: {url}",
                    r.status
                )))
                .with_span(span));
            }
            match r.body.take() {
                Some(b) => Ok(mk_input_stream(b)),
                None => Err(RjError::thrown(mk_io_exception("stream is closed".into())).with_span(span)),
            }
        }
        _ => Ok(Value::Nil),
    }
}

/// e2: `(RandomAccessFile. f mode)` -- "r" opens read-only, any "rw*" mode read+write+create (JVM semantics).
fn mk_random_access_file(path: &str, mode: &str, span: Span) -> Result<Value, RjError> {
    let mut oo = std::fs::OpenOptions::new();
    oo.read(true);
    match mode {
        "r" => {}
        "rw" | "rws" | "rwd" => {
            oo.write(true).create(true);
        }
        _ => {
            return Err(RjError::other(format!(
                "java.lang.IllegalArgumentException: Illegal mode \"{mode}\" must be one of \"r\", \"rw\", \"rws\", or \"rwd\""
            ))
            .with_span(span))
        }
    }
    let file = oo.open(path).map_err(|_| RjError::thrown(mk_file_not_found(path)).with_span(span))?;
    Ok(Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::RandomAccessFile,
        state: Mutex::new(HostState::OpenFile(Some(Arc::new(file)))),
    })))
}

/// e2: whole-file POSIX record lock (what the JVM's FileChannel.lock uses), so kondo's cache
/// lock interoperates with a JVM clj-kondo on the same `.clj-kondo/.cache`. `wait` = F_SETLKW.
fn fcntl_lock(file: &std::fs::File, lock_type: libc::c_short, wait: bool) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = lock_type;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    let cmd = if wait { libc::F_SETLKW } else { libc::F_SETLK };
    loop {
        if unsafe { libc::fcntl(file.as_raw_fd(), cmd, &mut fl) } != -1 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) | Some(libc::EACCES) if !wait => return Ok(false),
            _ => return Err(e),
        }
    }
}

/// e2: files this process holds a FileLock on -- fcntl locks are per-process, so an in-process
/// second `.tryLock` must fail here (the JVM throws OverlappingFileLockException; nil is the same to callers).
fn held_file_locks() -> &'static Mutex<Vec<(u64, u64)>> {
    static HELD: std::sync::OnceLock<Mutex<Vec<(u64, u64)>>> = std::sync::OnceLock::new();
    HELD.get_or_init(|| Mutex::new(Vec::new()))
}

fn file_key(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|m| (m.dev(), m.ino()))
}

fn call_file_lock_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    let closed = || RjError::other("java.nio.channels.ClosedChannelException").with_span(span);
    match (h.kind, method, args) {
        (HostKind::RandomAccessFile, "getChannel", []) => {
            let HostState::OpenFile(f) = &*crate::sync::lock_mutex(&h.state) else { unreachable!() };
            Ok(Value::HostInst(Arc::new(HostInstVal {
                kind: HostKind::FileChannel,
                state: Mutex::new(HostState::OpenFile(f.clone())),
            })))
        }
        (HostKind::RandomAccessFile | HostKind::FileChannel, "close", []) => {
            let mut g = crate::sync::lock_mutex(&h.state);
            if let HostState::OpenFile(f) = &mut *g {
                *f = None;
            }
            Ok(Value::Nil)
        }
        (HostKind::FileChannel, "isOpen", []) => {
            let HostState::OpenFile(f) = &*crate::sync::lock_mutex(&h.state) else { unreachable!() };
            Ok(Value::Bool(f.is_some()))
        }
        (HostKind::FileChannel, "tryLock" | "lock", []) => {
            let file = {
                let HostState::OpenFile(f) = &*crate::sync::lock_mutex(&h.state) else { unreachable!() };
                f.clone().ok_or_else(closed)?
            };
            let key = file_key(&file);
            let wait = method == "lock";
            loop {
                {
                    let mut held = crate::sync::lock_mutex(held_file_locks());
                    if !key.is_some_and(|k| held.contains(&k)) {
                        let got = fcntl_lock(&file, libc::F_WRLCK as libc::c_short, wait)
                            .map_err(|e| RjError::other(format!("java.io.IOException: {e}")).with_span(span))?;
                        if !got {
                            return Ok(Value::Nil);
                        }
                        if let Some(k) = key {
                            held.push(k);
                        }
                        return Ok(Value::HostInst(Arc::new(HostInstVal {
                            kind: HostKind::FileLock,
                            state: Mutex::new(HostState::FileLock(Some(file))),
                        })));
                    }
                }
                if !wait {
                    return Ok(Value::Nil);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        (HostKind::FileLock, "release" | "close", []) => {
            let mut g = crate::sync::lock_mutex(&h.state);
            if let HostState::FileLock(slot) = &mut *g {
                if let Some(file) = slot.take() {
                    let _ = fcntl_lock(&file, libc::F_UNLCK as libc::c_short, false);
                    if let Some(k) = file_key(&file) {
                        let mut held = crate::sync::lock_mutex(held_file_locks());
                        if let Some(i) = held.iter().position(|x| *x == k) {
                            held.swap_remove(i);
                        }
                    }
                }
            }
            Ok(Value::Nil)
        }
        (HostKind::FileLock, "isValid", []) => {
            let HostState::FileLock(slot) = &*crate::sync::lock_mutex(&h.state) else { unreachable!() };
            Ok(Value::Bool(slot.is_some()))
        }
        (_, m, a) => Err(RjError::other(format!(
            "no method {m} with {} args on {}",
            a.len(),
            crate::types::builtin_class_name(&Value::HostInst(h.clone()))
        ))
        .with_span(span)),
    }
}

fn mk_jar_file(path: &str, span: Span) -> Result<Value, RjError> {
    let file = std::fs::File::open(path).map_err(|_| RjError::thrown(mk_file_not_found(path)).with_span(span))?;
    let archive = zip::ZipArchive::new(file)
        .map_err(|e| RjError::other(format!("java.util.zip.ZipException: {path}: {e}")).with_span(span))?;
    Ok(Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::JarFile,
        state: Mutex::new(HostState::JarFile { archive: JarArchive(archive), closed: false }),
    })))
}

fn mk_jar_entry(index: usize, name: Str, is_dir: bool, size: u64) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::JarEntry,
        state: Mutex::new(HostState::JarEntry { index, name, is_dir, size }),
    }))
}

/// Cheap metadata-only scan (`by_index_raw`: reads the central-directory
/// record, does NOT inflate the entry) -- backs both `.entries` and
/// `.getEntry`/`.getJarEntry`'s linear name search.
fn jar_entries_raw(archive: &mut zip::ZipArchive<std::fs::File>) -> Vec<Value> {
    (0..archive.len())
        .filter_map(|i| {
            archive.by_index_raw(i).ok().map(|f| mk_jar_entry(i, Str::from(f.name().to_string()), f.is_dir(), f.size()))
        })
        .collect()
}

fn jar_file_closed_err(span: Span) -> RjError {
    RjError::other("java.io.IOException: zip file closed").with_span(span)
}

fn call_jar_file_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    match (method, args) {
        ("entries", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::JarFile { archive, closed } = &mut *guard else {
                unreachable!("HostKind::JarFile always carries HostState::JarFile")
            };
            if *closed {
                return Err(jar_file_closed_err(span));
            }
            let entries = jar_entries_raw(&mut archive.0);
            Ok(Value::HostInst(Arc::new(HostInstVal {
                kind: HostKind::JarEntries,
                state: Mutex::new(HostState::JarEntries { entries, pos: 0 }),
            })))
        }
        ("getEntry" | "getJarEntry", [Value::Str(name)]) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::JarFile { archive, closed } = &mut *guard else {
                unreachable!("HostKind::JarFile always carries HostState::JarFile")
            };
            if *closed {
                return Err(jar_file_closed_err(span));
            }
            let found = jar_entries_raw(&mut archive.0).into_iter().find(|v| {
                matches!(v, Value::HostInst(e) if matches!(&*crate::sync::lock_mutex(&e.state),
                    HostState::JarEntry { name: n, .. } if n.as_ref() == name.as_ref()))
            });
            Ok(found.unwrap_or(Value::Nil))
        }
        ("getInputStream", [Value::HostInst(entry)]) if entry.kind == HostKind::JarEntry => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::JarFile { archive, closed } = &mut *guard else {
                unreachable!("HostKind::JarFile always carries HostState::JarFile")
            };
            if *closed {
                return Err(jar_file_closed_err(span));
            }
            let index = {
                let HostState::JarEntry { index, .. } = &*crate::sync::lock_mutex(&entry.state) else {
                    unreachable!("HostKind::JarEntry always carries HostState::JarEntry")
                };
                *index
            };
            let mut zf = archive
                .0
                .by_index(index)
                .map_err(|e| RjError::other(format!("java.io.IOException: {e}")).with_span(span))?;
            let mut buf = Vec::with_capacity(zf.size() as usize);
            std::io::Read::read_to_end(&mut zf, &mut buf)
                .map_err(|e| RjError::other(format!("java.io.IOException: {e}")).with_span(span))?;
            Ok(mk_input_stream(Box::new(std::io::Cursor::new(buf))))
        }
        ("close", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::JarFile { closed, .. } = &mut *guard else {
                unreachable!("HostKind::JarFile always carries HostState::JarFile")
            };
            *closed = true;
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

fn call_jar_entry_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    let guard = crate::sync::lock_mutex(&h.state);
    let HostState::JarEntry { name, is_dir, size, .. } = &*guard else {
        unreachable!("HostKind::JarEntry always carries HostState::JarEntry")
    };
    match (method, args) {
        ("getName", []) => Ok(Value::Str(name.clone())),
        ("isDirectory", []) => Ok(Value::Bool(*is_dir)),
        ("getSize", []) => Ok(Value::Int(*size as i64)),
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// `Enumeration` surface for `(.entries jar)`'s result -- `enumeration-
/// seq` (`core.mova`) drives these two generically, same shape as
/// `HostKind::StringTokenizer`'s `hasMoreElements`/`nextElement`.
fn call_jar_entries_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::JarEntries { entries, pos } = &mut *guard else {
        unreachable!("HostKind::JarEntries always carries HostState::JarEntries")
    };
    match (method, args) {
        ("hasMoreElements", []) => Ok(Value::Bool(*pos < entries.len())),
        ("nextElement", []) => {
            if *pos >= entries.len() {
                return Err(RjError::other("java.util.NoSuchElementException").with_span(span));
            }
            let v = entries[*pos].clone();
            *pos += 1;
            Ok(v)
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

fn current_epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `(.instant clock)` -- see `HostKind::Instant`'s doc.
pub(crate) fn mk_instant(epoch_millis: i64) -> Value {
    Value::HostInst(Arc::new(HostInstVal { kind: HostKind::Instant, state: Mutex::new(HostState::Instant(epoch_millis)) }))
}

fn call_clock_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    match (method, args) {
        ("instant", []) => Ok(mk_instant(current_epoch_millis())),
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

fn call_instant_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    let HostState::Instant(millis) = &*crate::sync::lock_mutex(&h.state) else {
        unreachable!("HostKind::Instant always carries HostState::Instant")
    };
    let millis = *millis;
    match (method, args) {
        ("toEpochMilli", []) => Ok(Value::Int(millis)),
        // Real `Instant.truncatedTo(ChronoUnit)` -- identity here, see
        // `HostState::Instant`'s doc for why (the arg, a `ChronoUnit`
        // enum value, is unused).
        ("truncatedTo", [_unit]) => Ok(Value::HostInst(h.clone())),
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

fn stream_io_err(syscall: &'static str, e: std::io::Error) -> RjError {
    // an error that is not an OS error (a closed stream) is the JVM's own `IOException` text
    let msg = if e.raw_os_error().is_none() { e.to_string() } else { format!("stream {syscall}: {e}") };
    RjError::sys(msg, e.raw_os_error().unwrap_or(libc::EIO), syscall)
}

/// Reads one byte -- real `InputStream.read()`'s contract: the byte
/// value (0..=255) as an `i64`, or `-1` at EOF.
pub(crate) fn stream_read_byte(h: &Arc<HostInstVal>) -> Result<i64, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::InputStream(r) = &mut *guard else {
        return Err(RjError::type_err("expected an input stream"));
    };
    let mut b = [0u8; 1];
    match r.0.read(&mut b) {
        Ok(0) => Ok(-1),
        Ok(_) => Ok(b[0] as i64),
        Err(e) => Err(stream_io_err("read", e)),
    }
}

/// Reads the text of ONE form off `h` and stops right after it: what is left
/// stays in the stream (`(read)` on an nREPL `*in*`). A bare token such as a
/// symbol ends at the next delimiter, which is peeked, not consumed. `None` at
/// EOF with nothing but whitespace read. At EOF in the middle of a form the
/// text read so far is returned (the caller's reader reports the error).
pub(crate) fn stream_read_form_text(h: &Arc<HostInstVal>) -> Result<Option<String>, RjError> {
    use std::io::BufRead;
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::InputStream(r) = &mut *guard else {
        return Err(RjError::type_err("expected an input stream"));
    };
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let mut b = [0u8; 1];
        match r.0.read(&mut b) {
            Ok(0) => {
                return Ok(String::from_utf8(acc).ok().filter(|t| !t.trim().is_empty()));
            }
            Ok(_) => acc.push(b[0]),
            Err(e) => return Err(stream_io_err("read", e)),
        }
        let Ok(text) = std::str::from_utf8(&acc) else { continue };
        let t = text.trim_start();
        if t.is_empty() {
            acc.clear();
            continue;
        }
        let mut rd = crate::reader::Reader::new(t);
        match rd.next_form() {
            Ok(Some(_)) => {
                if t.starts_with(['(', '[', '{', '"']) || t.starts_with("#{") || t.starts_with("#(") {
                    return Ok(Some(t.to_string()));
                }
                // a bare token: done when the next byte is a delimiter or the stream ends
                match r.0.fill_buf() {
                    Ok(buf) if buf.is_empty() => return Ok(Some(t.to_string())),
                    Ok(buf) if matches!(buf[0], b' ' | b'\t' | b'\n' | b'\r' | b',' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"' | b';') => {
                        return Ok(Some(t.to_string()))
                    }
                    Ok(_) => {}
                    Err(e) => return Err(stream_io_err("read", e)),
                }
            }
            Ok(None) => {}
            Err(e) => {
                let m = e.message.to_lowercase();
                if !(m.contains("unclosed") || m.contains("eof") || m.contains("unterminated")) {
                    return Ok(Some(t.to_string()));
                }
            }
        }
    }
}

/// nREPL `skip-stdin-newline`: after an eval, a newline that is the next
/// buffered character of `*in*` is dropped (the client sent `"(1 2)\n"` and
/// `(read)` took the form). Looks only at what is buffered; never blocks.
pub(crate) fn stream_skip_buffered_newline(h: &Arc<HostInstVal>) {
    use std::io::BufRead;
    let mut guard = crate::sync::lock_mutex(&h.state);
    if let HostState::InputStream(r) = &mut *guard {
        if r.0.buffer().first() == Some(&b'\n') {
            r.0.consume(1);
        }
    }
}

/// Reads one line off `h`, LF-terminated (a bare `\r` is swallowed, not
/// appended -- matches the JVM `InputStream.read()` header-line loop
/// `jsonrpc4clj.io-chan/read-header-line` uses). `None` at a clean EOF
/// (no bytes read at all); an EOF reached mid-line still returns the
/// partial line as `Some`, matching the original JVM code's own
/// `Ok(0) => return line`-when-non-empty shape.
pub(crate) fn stream_read_line(h: &Arc<HostInstVal>) -> Result<Option<Str>, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::InputStream(r) = &mut *guard else {
        return Err(RjError::type_err("expected an input stream"));
    };
    let mut line = String::new();
    let mut byte = [0u8; 1];
    loop {
        match r.0.read(&mut byte) {
            Ok(0) => return Ok(if line.is_empty() { None } else { Some(Str::from(line)) }),
            Ok(_) => match byte[0] {
                b'\n' => return Ok(Some(Str::from(line))),
                b'\r' => continue,
                b => line.push(b as char),
            },
            Err(e) => return Err(stream_io_err("read", e)),
        }
    }
}

/// Reads EXACTLY `n` bytes (looping on short reads, matching real
/// `InputStream.read(buf, off, len)`'s contract) and UTF-8-decodes them.
/// `None` only for a CLEAN EOF (zero bytes available before any of the
/// `n` were read); an EOF reached after some but not all of `n` is a
/// real, catchable error -- a genuinely truncated frame, not silently
/// swallowed (mirrors the JVM original's `EOFException`).
pub(crate) fn stream_read_n_bytes(h: &Arc<HostInstVal>, n: usize) -> Result<Option<Str>, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::InputStream(r) = &mut *guard else {
        return Err(RjError::type_err("expected an input stream"));
    };
    let mut buf = vec![0u8; n];
    let mut total = 0usize;
    while total < n {
        match r.0.read(&mut buf[total..]) {
            Ok(0) if total == 0 => return Ok(None),
            Ok(0) => {
                return Err(RjError::other(format!(
                    "unexpected EOF after {total} of {n} byte(s)"
                )))
            }
            Ok(k) => total += k,
            Err(e) => return Err(stream_io_err("read", e)),
        }
    }
    Ok(Some(Str::from(String::from_utf8_lossy(&buf).into_owned())))
}

/// Reads `h` to EOF and returns everything read (UTF-8 decoded), even
/// `""` for an already-empty/exhausted stream -- unlike
/// `stream_read_n_bytes`, a short/empty read is success here, not an
/// error, since there is no target length to fall short of. Added for
/// `cognitect.transit/reader`'s shim (mova/shims/cognitect/transit.mova),
/// which needs the WHOLE stream up front (transit-json's cache table is
/// stateful across the whole document, so it cannot be decoded
/// incrementally the way `read-line-bytes` reads JSON-RPC frames).
pub(crate) fn stream_read_all(h: &Arc<HostInstVal>) -> Result<Str, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::InputStream(r) = &mut *guard else {
        return Err(RjError::type_err("expected an input stream"));
    };
    let mut buf = Vec::new();
    r.0.read_to_end(&mut buf).map_err(|e| stream_io_err("read", e))?;
    Ok(Str::from(String::from_utf8_lossy(&buf).into_owned()))
}

/// `mova.transit/read-json-file`'s entry point: streams transit-json
/// straight off `h` (`crate::builtins::transit::read_json_reader`) --
/// unlike `stream_read_all` above, the whole document is never
/// materialized as one `Str` alongside the `Value` tree it decodes to.
pub(crate) fn stream_read_json(h: &Arc<HostInstVal>) -> Result<crate::value::Value, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::InputStream(r) = &mut *guard else {
        return Err(RjError::type_err("expected an input stream"));
    };
    crate::builtins::read_json_reader(&mut r.0)
}

/// Runs `f` against `h`'s buffered output sink (`mova.transit/write-json-file`).
pub(crate) fn with_output_stream<R>(
    h: &Arc<HostInstVal>,
    f: impl FnOnce(&mut dyn std::io::Write) -> Result<R, RjError>,
) -> Result<R, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::OutputStream(w) = &mut *guard else {
        return Err(RjError::type_err("expected an output stream"));
    };
    f(&mut w.0)
}

/// Writes `s`'s UTF-8 bytes straight through to `h` (no line/frame
/// structure of its own -- the caller, e.g. the jsonrpc4clj overlay's
/// `write-message`, builds the full header+body string first and calls
/// this once per frame).
pub(crate) fn stream_write_str(h: &Arc<HostInstVal>, s: &str) -> Result<(), RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::OutputStream(w) = &mut *guard else {
        return Err(RjError::type_err("expected an output stream"));
    };
    w.0.write_all(s.as_bytes()).map_err(|e| stream_io_err("write", e))
}

/// Flushes an output stream's `BufWriter`; a no-op on an input stream
/// (matches real `Reader.flush()`/`InputStream` having no such method at
/// all -- callers that flush generically, like `with-open`'s expansion
/// never does but a defensive caller might, get a harmless success
/// rather than a spurious error).
pub(crate) fn stream_flush(h: &Arc<HostInstVal>) -> Result<(), RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    match &mut *guard {
        HostState::OutputStream(w) => w.0.flush().map_err(|e| stream_io_err("write", e)),
        HostState::InputStream(_) => Ok(()),
        _ => Err(RjError::type_err("expected a stream")),
    }
}

/// Closes a stream: flushes first (for an output stream), then replaces
/// the boxed source/sink with a stub (`std::io::empty()`/`std::io::
/// sink()`) so the real underlying fd (a file's, in particular) is
/// actually dropped and released NOW, not whenever the `Arc<HostInstVal>`
/// happens to be GC'd -- `with-open`'s whole point. A stream closed twice,
/// or read/written after close, harmlessly hits EOF/a no-op sink rather
/// than erroring (matches real Java: `close()` is idempotent, though a
/// post-close read/write there DOES throw -- narrower here since nothing
/// in this campaign's scope does that on purpose).
pub(crate) fn stream_close(h: &Arc<HostInstVal>) -> Result<(), RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    if let HostState::OutputStream(w) = &mut *guard {
        let _ = w.0.flush();
    }
    *guard = match &*guard {
        HostState::InputStream(_) => {
            HostState::InputStream(BoxedReader(std::io::BufReader::new(Box::new(ClosedReader))))
        }
        HostState::OutputStream(_) => {
            HostState::OutputStream(BoxedWriter(std::io::BufWriter::new(Box::new(std::io::sink()))))
        }
        _ => return Err(RjError::type_err("expected a stream")),
    };
    Ok(())
}

/// `.read`/`.readLine`/`.flush`/`.close` on a `HostKind::InputStream`.
fn call_input_stream_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match (method, args) {
        ("read", []) => Ok(Value::Int(stream_read_byte(h)?)),
        ("readLine", []) => Ok(match stream_read_line(h)? {
            Some(s) => Value::Str(s),
            None => Value::Nil,
        }),
        ("flush", []) => {
            stream_flush(h)?;
            Ok(Value::Nil)
        }
        ("close", []) => {
            stream_close(h)?;
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// `.write`/`.append`/`.flush`/`.close` on a `HostKind::OutputStream`.
fn call_output_stream_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match (method, args) {
        ("write", [v]) | ("append", [v]) => {
            let s = match v {
                Value::Str(s) => s.as_ref().to_string(),
                other => crate::printer::pr_str(other),
            };
            stream_write_str(h, &s)?;
            Ok(Value::Nil)
        }
        // `PrintStream`: `(.println System/out x)` writes `str` of x and a newline, and
        // flushes (System/out is auto-flushing); `.print` does not flush.
        ("println", []) => {
            stream_write_str(h, "\n")?;
            stream_flush(h)?;
            Ok(Value::Nil)
        }
        ("println", [v]) => {
            stream_write_str(h, &format!("{}\n", crate::printer::display_str(v)))?;
            stream_flush(h)?;
            Ok(Value::Nil)
        }
        ("print", [v]) => {
            stream_write_str(h, &crate::printer::display_str(v))?;
            Ok(Value::Nil)
        }
        ("flush", []) => {
            stream_flush(h)?;
            Ok(Value::Nil)
        }
        ("close", []) => {
            stream_close(h)?;
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}


/// W4-veneer: splits `content` into lines the way real
/// `BufferedReader.readLine()` does -- terminator-agnostic w.r.t. a
/// trailing newline (measured against the oracle: a file ending in `\n`
/// yields the SAME lines as the same content without it -- no spurious
/// trailing empty line) and `"a\n\nb\n"` yields `["a" "" "b"]` (an
/// embedded blank line is a real, counted empty line). Rust's
/// `str::lines()` matches this exactly for `\n`/`\r\n`-terminated
/// content, which is everything this corpus's fixture files use; a lone
/// `\r` (old Mac line endings, which real `BufferedReader` also treats as
/// a terminator) is NOT split by `str::lines()` -- undisclosed only
/// because nothing in scope has one (measured: `readme.txt` and every
/// vendored fixture use `\n` exclusively).
fn split_lines(content: &str) -> Vec<Str> {
    content.lines().map(Str::from).collect()
}

/// W4-veneer: `Files/newBufferedReader` -- opens `path` (a `java.io.File`/
/// `Path`-conflated `HostInst`, see `HostKind::JavaFile`) and reads its
/// ENTIRE content eagerly (see `HostState::BufferedReader`'s doc for why
/// that is honest here). A missing/unreadable file is an ordinary,
/// catchable `RjError` -- real `Files.newBufferedReader` throws
/// `java.nio.file.NoSuchFileException`/`IOException`, but nothing in this
/// corpus opens a missing path through this call (the ONE vendored call
/// site, `sequences.clj`'s `test-iteration`, always opens a real,
/// present `readme.txt`), so no exception-class taxonomy is minted for
/// it -- narrower scope than `java.io.FileReader`'s own `.
/// FileNotFoundException`, which IS measured/needed (see try_catch.clj's
/// deftest and `mk_file_not_found`).
pub(crate) fn new_buffered_reader(path_arg: &Value) -> Result<Value, RjError> {
    let path = match path_arg {
        Value::HostInst(h) if h.kind == HostKind::JavaFile => {
            let HostState::JavaFile(p) = &*crate::sync::lock_mutex(&h.state) else {
                unreachable!("HostKind::JavaFile always carries HostState::JavaFile")
            };
            p.clone()
        }
        Value::Str(s) => s.clone(),
        other => {
            return Err(RjError::type_err(format!(
                "java.nio.file.Files/newBufferedReader: expected a Path/File, got {}",
                other.type_name()
            )))
        }
    };
    let content = std::fs::read_to_string(path.as_ref()).map_err(|e| {
        RjError::other(format!(
            "java.nio.file.Files/newBufferedReader: could not read {}: {e}",
            path.as_ref()
        ))
    })?;
    Ok(Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::BufferedReader,
        state: Mutex::new(HostState::BufferedReader { lines: split_lines(&content), pos: 0, closed: false }),
    })))
}

/// `java.nio.file.Files/newBufferedReader` -- registered by `register`
/// below as the qualified global `Symbol { ns: Some("java.nio.file.
/// Files"), name: "newBufferedReader" }`. Arity-checked here (mirrors
/// `builtins::statics::reg_static_fn`'s generated check; not reused
/// directly since that fn is private to its own module).
fn files_new_buffered_reader(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match args {
        [path] => new_buffered_reader(path),
        _ => Err(RjError::arity(format!(
            "java.nio.file.Files/newBufferedReader: expected exactly 1 arg, got {}",
            args.len()
        ))),
    }
}

/// `ReflectorTryCatchFixture/fail` -- the two STATIC overloads
/// `.oracle/clojure-src/test/java/clojure/test/ReflectorTryCatchFixture.java`
/// declares (`fail(Long)`/`fail(Double)`), dispatched by the ACTUAL
/// runtime type of the one arg the vendored `(defn fail [x]
/// (ReflectorTryCatchFixture/fail x))` passes -- exactly what real
/// Java's overload reflection does here too (boxed `Long`/`Double`
/// picked by the boxed arg's own class), so this is a narrow, honest
/// hand-transcription of the fixture's own two bodies, not invented
/// general reflection.
fn reflector_fixture_fail(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match args {
        [Value::Int(_)] => Err(RjError::thrown(mk_cookies("Long", None))),
        [Value::Float(_)] => Err(RjError::thrown(mk_cookies("Double", None))),
        [other] => Err(RjError::type_err(format!(
            "ReflectorTryCatchFixture/fail: expected a Long or Double, got {}",
            other.type_name()
        ))),
        _ => Err(RjError::arity(format!(
            "ReflectorTryCatchFixture/fail: expected exactly 1 arg, got {}",
            args.len()
        ))),
    }
}

/// W4-veneer: `.readLine`/`.close` -- see `HostState::BufferedReader`'s
/// doc. `.readLine` past the last line (or on an already-`.close`d
/// reader -- `closed` is tracked but not enforced, see that doc) returns
/// `Value::Nil`, matching real Java's `null` and `line-seq`'s own
/// termination contract.
fn call_bufferedreader_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match (method, args) {
        ("readLine", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::BufferedReader { lines, pos, .. } = &mut *guard else {
                unreachable!("HostKind::BufferedReader always carries HostState::BufferedReader")
            };
            if *pos < lines.len() {
                let line = lines[*pos].clone();
                *pos += 1;
                Ok(Value::Str(line))
            } else {
                Ok(Value::Nil)
            }
        }
        ("close", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::BufferedReader { closed, .. } = &mut *guard else {
                unreachable!("HostKind::BufferedReader always carries HostState::BufferedReader")
            };
            *closed = true;
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// lsp/io: `.readLine` on a `HostState::StringReader` -- terminator-
/// stripping (`\n` or `\r\n`) read from `pos` to the next line break, or
/// to end-of-content on the last (possibly unterminated) line; `Value::
/// Nil` once `pos` has reached the end, matching `call_bufferedreader_
/// method`'s `readLine` contract (and `clojure.lang.
/// LineNumberingPushbackReader`'s real one, which `clojure.core/read-
/// line` calls when `*in*` is one). `read_line_from_string_reader` is the
/// same logic reused by `builtins::sys::read_line` for `(read-line)`
/// against a `with-in-str`-bound `*in*`.
fn call_string_reader_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match (method, args) {
        ("readLine", []) => read_line_from_string_reader(h),
        ("close", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::StringReader { closed, .. } = &mut *guard else {
                unreachable!("HostKind::StringReader always carries HostState::StringReader")
            };
            *closed = true;
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// Shared by `call_string_reader_method`'s `"readLine"` arm and
/// `builtins::sys::read_line`'s `*in*`-bound branch -- one cursor
/// advance, byte-exact (see `HostState::StringReader`'s doc for why the
/// cursor is always a valid UTF-8 boundary).
pub(crate) fn read_line_from_string_reader(h: &Arc<HostInstVal>) -> Result<Value, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::StringReader { content, pos, .. } = &mut *guard else {
        unreachable!("HostKind::StringReader always carries HostState::StringReader")
    };
    let text = content.as_ref();
    if *pos >= text.len() {
        return Ok(Value::Nil);
    }
    let rest = &text[*pos..];
    match rest.find('\n') {
        Some(i) => {
            let line = rest[..i].strip_suffix('\r').unwrap_or(&rest[..i]);
            let line = Str::from(line);
            *pos += i + 1;
            Ok(Value::Str(line))
        }
        None => {
            let line = Str::from(rest);
            *pos = text.len();
            Ok(Value::Str(line))
        }
    }
}

/// lsp/io: `slurp` of a `HostState::StringReader` -- the WHOLE remaining
/// content from `pos` to end, in one shot (mirrors real `slurp`'s "drain
/// the reader" contract; `clj_kondo.impl.core/process-file`'s `(= "-"
/// path)` branch is the one call site, `(slurp *in*)`). Draining sets
/// `pos` to the end, same "exhausted" state `.readLine` past EOF checks.
pub(crate) fn slurp_string_reader(h: &Arc<HostInstVal>) -> Result<Value, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::StringReader { content, pos, .. } = &mut *guard else {
        unreachable!("HostKind::StringReader always carries HostState::StringReader")
    };
    let text = content.as_ref();
    let remaining = if *pos >= text.len() { "" } else { &text[*pos..] };
    let result = Str::from(remaining);
    *pos = text.len();
    Ok(Value::Str(result))
}

/// W4-veneer (try_catch.clj's `catch-receives-checked-exception-from-
/// reflective-call`): `.failWithCause` -- the ONE instance method
/// `ReflectorTryCatchFixture` (`.oracle/clojure-src/test/java/clojure/
/// test/ReflectorTryCatchFixture.java`) declares, transcribed exactly:
/// `public void failWithCause(Double y) throws Cookies { throw new
/// Cookies("Wrapped", new Cookies("Cause")); }` -- takes one arg (the
/// vendored call always passes a `Double`, matching the real overload;
/// no `Long` overload of `failWithCause` exists on the Java source, so
/// none is modeled here), always throws the SAME two-deep `Cookies`
/// chain regardless of the arg's actual value (the fixture's own source
/// ignores its argument entirely).
fn call_reflector_fixture_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match (method, args) {
        ("failWithCause", [Value::Float(_)]) => {
            let cause = mk_cookies("Cause", None);
            Err(RjError::thrown(mk_cookies("Wrapped", Some(cause))).with_span(span))
        }
        ("failWithCause", [other]) => Err(RjError::type_err(format!(
            "ReflectorTryCatchFixture.failWithCause: expected a Double, got {}",
            other.type_name()
        ))
        .with_span(span)),
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// D1: `(.spliterator v)` -- a cursor over `items`, owning `[pos, end)`.
/// Built by `builtins::vecdot`'s `.spliterator` row; see `HostKind::
/// Spliterator` for the five-method scope and why it is exactly five.
pub fn mk_spliterator(items: crate::value::PVec) -> Value {
    let end = items.len();
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Spliterator,
        state: Mutex::new(HostState::Spliterator { items, pos: 0, end }),
    }))
}

/// D1: `(.stream v)`/`(.parallelStream v)` -- see `HostKind::Stream`.
pub fn mk_stream(items: crate::value::PVec) -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Stream,
        state: Mutex::new(HostState::Stream(items)),
    }))
}

/// D1: `(Collectors/counting)` -- see `HostKind::Collector`.
pub fn mk_counting_collector() -> Value {
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::Collector,
        state: Mutex::new(HostState::Collector),
    }))
}

/// D1: hands one element to a `java.util.function.Consumer`. The consumer
/// is whatever the caller passed -- in the vendored suite always a
/// `(reify Consumer (accept [_ v] ...))`, so this is an ordinary
/// `.accept` interop call routed through the same
/// `lookup_interface_method` `eval_dot_form` uses; any other callable
/// shape (a plain fn) works too, since `apply_value` is the fallback.
fn accept_one(
    interp: &mut Interp,
    consumer: &Value,
    item: Value,
    span: Span,
) -> Result<(), RjError> {
    if let Value::Inst(inst) = consumer {
        if let Some(f) =
            crate::builtins::types::lookup_interface_method(&interp.interfaces, inst, "accept")
        {
            interp.apply_value(&f, &[consumer.clone(), item], span)?;
            return Ok(());
        }
        return Err(RjError::other(format!(
            "accept: no method .accept on {}",
            inst.tdef.name
        ))
        .with_span(span));
    }
    interp.apply_value(consumer, &[item], span)?;
    Ok(())
}

/// D1: `.estimateSize`/`.getExactSizeIfKnown`/`.tryAdvance`/`.trySplit`/
/// `.forEachRemaining`.
///
/// Every arm takes the element (and advances) UNDER the state lock, then
/// RELEASES it before calling back into the interpreter -- a consumer is
/// arbitrary user code and may touch this very spliterator.
///
/// `trySplit`'s exact halving is deliberately not a promise: the vendored
/// `test-spliterator-trySplit` splits recursively until every split
/// refuses, then asserts the UNION of everything walked is the whole
/// vector (`(= v (sort @seen))` over a set) -- split shape is
/// unobservable, so "first half off, keep the rest" is as good as the
/// real JVM's and needs no oracle measurement.
fn call_spliterator_method(
    interp: &mut Interp,
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    /// The one place that knows a `HostKind::Spliterator` always holds a
    /// `HostState::Spliterator` -- keeps every arm below a two-liner.
    fn parts(guard: &mut HostState) -> (&mut crate::value::PVec, &mut usize, &mut usize) {
        let HostState::Spliterator { items, pos, end } = guard else {
            unreachable!("HostKind::Spliterator always holds HostState::Spliterator");
        };
        (items, pos, end)
    }
    /// Takes the next element and advances, or `None` when drained.
    fn take_next(h: &Arc<HostInstVal>) -> Option<Value> {
        let mut guard = crate::sync::lock_mutex(&h.state);
        let (items, pos, end) = parts(&mut guard);
        if *pos >= *end {
            return None;
        }
        let item = items.get_owned(*pos);
        *pos += 1;
        item
    }
    match (method, args) {
        // Real `Spliterator.estimateSize()` is an ESTIMATE in general,
        // but a sized one (which a vector's is) reports exactly the
        // remaining count -- and `getExactSizeIfKnown` reports the same
        // number for a SIZED spliterator, which is why the vendored test
        // asserts the two are `=` to each other and to 0 on an empty
        // vector.
        ("estimateSize", []) | ("getExactSizeIfKnown", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let (_, pos, end) = parts(&mut guard);
            Ok(Value::Int((*end - *pos) as i64))
        }
        ("tryAdvance", [consumer]) => match take_next(h) {
            Some(item) => {
                accept_one(interp, consumer, item, span)?;
                Ok(Value::Bool(true))
            }
            None => Ok(Value::Bool(false)),
        },
        ("forEachRemaining", [consumer]) => {
            while let Some(item) = take_next(h) {
                accept_one(interp, consumer, item, span)?;
            }
            Ok(Value::Nil)
        }
        // `null` when this cursor cannot be split (real contract: fewer
        // than two elements left, or the source refuses) -- the vendored
        // test's recursion terminates on exactly that.
        ("trySplit", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let (items, pos, end) = parts(&mut guard);
            if *end - *pos < 2 {
                return Ok(Value::Nil);
            }
            let mid = *pos + (*end - *pos) / 2;
            let split = Value::HostInst(Arc::new(HostInstVal {
                kind: HostKind::Spliterator,
                state: Mutex::new(HostState::Spliterator {
                    items: items.clone(),
                    pos: *pos,
                    end: mid,
                }),
            }));
            *pos = mid;
            Ok(split)
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// D1: `.collect` -- the stream veneer's ONE method (see
/// `HostKind::Stream`). Only a `(Collectors/counting)` marker is
/// accepted; anything else is an honest "not implemented", never a
/// silently wrong count.
fn call_stream_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match (method, args) {
        ("collect", [collector]) => {
            if !matches!(collector, Value::HostInst(c) if c.kind == HostKind::Collector) {
                return Err(RjError::type_err(
                    "collect: mova implements exactly one collector, (Collectors/counting)",
                )
                .with_span(span));
            }
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::Stream(items) = &*guard else {
                unreachable!("HostKind::Stream always holds HostState::Stream");
            };
            Ok(Value::Int(items.len() as i64))
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// `.hasNext`/`.next` -- `java.util.Iterator`'s two methods this task's
/// scope needs (real `Iterator` also has `.remove`, unsupported like
/// every other unmeasured method here). `.next` on an exhausted iterator
/// throws (measured: real `NoSuchElementException`, message not
/// corpus-scored -- `test-seq-iter-match`'s own `seq-iter-match` helper
/// only ever calls `.next` after checking `.hasNext`, so the exact
/// message on the throwing path is never observed by the suite either
/// way).
fn call_iterator_method(_interp: &mut Interp, h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    match (method, args) {
        ("hasNext", []) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::Iterator(remaining) = &*guard else {
                unreachable!("HostKind::Iterator always holds HostState::Iterator");
            };
            Ok(Value::Bool(!matches!(remaining, Value::Nil)))
        }
        ("next", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::Iterator(remaining) = &mut *guard else {
                unreachable!("HostKind::Iterator always holds HostState::Iterator");
            };
            match remaining {
                Value::List(items) if !items.is_empty() => {
                    let mut items = items.clone();
                    let head = items.pop_front().expect("checked non-empty above");
                    // C10: normalize an exhausted tail back to `Nil` --
                    // `Value::List(PVec::new())` is NOT `Value::Nil`, and
                    // `.hasNext`'s check is a bare `!matches!(remaining,
                    // Nil)`, so leaving an empty-but-still-`List` here
                    // made `.hasNext` wrongly stay `true` forever after
                    // the last element (measured root cause: `(.iterator
                    // (array-map nil 1))`'s single element consumed, then
                    // `.hasNext` still `true` -- `test-seq-iter-match`'s
                    // "Seq exhausted before iterator" failure).
                    *remaining = if items.is_empty() { Value::Nil } else { Value::List(items) };
                    Ok(head)
                }
                _ => Err(RjError::other("iterator exhausted (NoSuchElementException)").with_span(span)),
            }
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// clojure-lsp campaign (mova/PLAN.md): `StringBuilder`/`StringBuffer`'s
/// method surface -- the ONE `java.*` veneer this campaign owns (mova/
/// PLAN.md's "java.* veneer" rule, and `HostKind`'s doc: previously
/// construction-only because "nothing in the vendored suite calls one",
/// which stopped being true the moment `rewrite-clj.reader`'s character-
/// at-a-time token buffer entered the picture -- `read-char`-level code
/// this campaign's own `parse-string-all`/`z/of-string` run through on
/// EVERY token). Kept deliberately narrow to what that buffer (and
/// `.toString`/`.length`-style read-back) needs -- a real Java
/// `StringBuilder` has many more overloads (`append` alone has ~13),
/// none of which any known call site in this campaign or the existing
/// vendored suite reaches:
///
/// - `.append` (char, String, or any other value via `str`-style
///   coercion -- real `StringBuilder.append(Object)` calls `.toString()`
///   on its arg, so a bare `nil` prints `"null"`, matching real Java)
///   mutates in place and returns `this` (real Java's fluent chaining
///   contract -- `rewrite-clj.reader`'s `(.append buf (char c))` result
///   is discarded, but nothing here should assume that).
/// - `.toString` reads the accumulated content back out as a `Value::
///   Str` -- unlike the old construction-only behaviour, this now
///   reflects every `.append` since construction, which is the entire
///   point of a MUTABLE buffer.
/// - `.length`/`.charAt`/`.deleteCharAt`/`.substring`: cheap, genuinely
///   general `CharSequence`-shaped reads/edits (`clojure.tools.reader.
///   reader-types`' source-logging reader calls all four on its own
///   internal log buffer), not a hack for one call site.
fn call_charbuf_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    match (method, args) {
        ("append", [v]) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::CharBuf(s) = &mut *guard else {
                unreachable!("StringBuilder/StringBuffer HostInst always holds HostState::CharBuf");
            };
            // Real `StringBuilder.append(Object)` semantics: a char
            // appends itself, everything else appends its `.toString()`
            // -- which for mova's print family IS `str`'s own coercion
            // (`nil` -> `"null"` on the JVM is the one place `str` and
            // `.append` diverge, `str` giving `""` -- `append` never
            // sees a bare `nil` in this campaign's own call sites, so
            // that divergence is left unmeasured rather than guessed at).
            let appended = match v {
                Value::Char(c) => c.to_string(),
                _ => crate::printer::display_str(v),
            };
            *s = Str::from(format!("{s}{appended}"));
            Ok(Value::HostInst(h.clone()))
        }
        ("toString", []) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::CharBuf(s) = &*guard else {
                unreachable!("StringBuilder/StringBuffer HostInst always holds HostState::CharBuf");
            };
            Ok(Value::Str(s.clone()))
        }
        ("length", []) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::CharBuf(s) = &*guard else {
                unreachable!("StringBuilder/StringBuffer HostInst always holds HostState::CharBuf");
            };
            Ok(Value::Int(s.chars().count() as i64))
        }
        ("charAt", [Value::Int(i)]) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::CharBuf(s) = &*guard else {
                unreachable!("StringBuilder/StringBuffer HostInst always holds HostState::CharBuf");
            };
            s.chars().nth(*i as usize).map(Value::Char).ok_or_else(|| {
                RjError::other(format!("charAt: index {i} out of bounds")).with_span(span)
            })
        }
        ("deleteCharAt", [Value::Int(i)]) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::CharBuf(s) = &mut *guard else {
                unreachable!("StringBuilder/StringBuffer HostInst always holds HostState::CharBuf");
            };
            let idx = *i as usize;
            let mut chars: Vec<char> = s.chars().collect();
            if idx >= chars.len() {
                return Err(
                    RjError::other(format!("deleteCharAt: index {i} out of bounds")).with_span(span)
                );
            }
            chars.remove(idx);
            *s = Str::from(chars.into_iter().collect::<String>());
            Ok(Value::HostInst(h.clone()))
        }
        ("substring", [Value::Int(from)]) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::CharBuf(s) = &*guard else {
                unreachable!("StringBuilder/StringBuffer HostInst always holds HostState::CharBuf");
            };
            let chars: Vec<char> = s.chars().collect();
            let idx = *from as usize;
            if idx > chars.len() {
                return Err(
                    RjError::other(format!("substring: index {from} out of bounds")).with_span(span)
                );
            }
            Ok(Value::Str(Str::from(chars[idx..].iter().collect::<String>())))
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

/// C10 + lsp/host: `java.util.HashMap`'s methods -- `.put` and `.get`
/// (see `HostKind::HashMap`'s doc for why nothing else is wired). `.get`
/// added for clojure-lsp's `canonicalize-java-analysis` value-interning
/// pool (`kondo.clj`'s `canonical-fn`), which round-trips arbitrary
/// Clojure values (strings/vectors/keywords) as keys. Both mirror real
/// `Map.put`/`Map.get`'s return value: `nil` when there's no mapping.
fn call_hashmap_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    match (method, args) {
        ("put", [k, v]) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::HashMap(m) = &mut *guard else {
                unreachable!("HashKind::HashMap always holds HostState::HashMap");
            };
            Ok(m.insert(k.clone(), v.clone()).unwrap_or(Value::Nil))
        }
        ("get", [k]) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::HashMap(m) = &*guard else {
                unreachable!("HashKind::HashMap always holds HostState::HashMap");
            };
            Ok(m.get(k).cloned().unwrap_or(Value::Nil))
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

fn call_random_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    let mut guard = crate::sync::lock_mutex(&h.state);
    let HostState::Random(r) = &mut *guard else {
        unreachable!("HostKind::Random cell always holds HostState::Random")
    };
    match (method, args) {
        ("nextInt", []) => Ok(Value::Int(next_int(&mut r.seed) as i64)),
        ("nextInt", [Value::Int(bound)]) => {
            let bound32 = i32::try_from(*bound).map_err(|_| {
                RjError::type_err("nextInt: bound out of int range".to_string()).with_span(span)
            })?;
            Ok(Value::Int(
                next_int_bound(&mut r.seed, bound32, span)? as i64
            ))
        }
        ("nextLong", []) => Ok(Value::Int(next_long(&mut r.seed))),
        ("nextDouble", []) => Ok(Value::Float(next_double(&mut r.seed))),
        ("nextFloat", []) => Ok(Value::Float(next_float(&mut r.seed))),
        ("nextBoolean", []) => Ok(Value::Bool(next_boolean(&mut r.seed))),
        ("setSeed", [Value::Int(seed)]) => {
            r.seed = scramble(*seed);
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(HostKind::Random, method, args.len(), span)),
    }
}

fn call_date_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    let guard = crate::sync::lock_mutex(&h.state);
    let HostState::Date(millis) = &*guard else {
        unreachable!("HostKind::Date cell always holds HostState::Date")
    };
    match (method, args) {
        ("getTime", []) => Ok(Value::Int(*millis)),
        _ => Err(unknown_method(HostKind::Date, method, args.len(), span)),
    }
}

/// 64 MiB, matching `builtins::conc::FUTURE_STACK_SIZE`'s own reasoning
/// exactly (a `(Thread. f)`'s `f` runs the SAME tree-walking recursive-
/// descent evaluator `future*`'s spawned closure does) -- duplicated
/// rather than exported because the two constants live in different,
/// otherwise-unrelated modules and neither owns the other.
const THREAD_STACK_SIZE: usize = 64 * 1024 * 1024;

fn call_thread_method(
    interp: &mut Interp,
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    debug_assert!(matches!(h.kind, HostKind::Thread));
    // C3c: `.start`/`.join` only apply to a `(Thread. f)`-constructed
    // instance (`HostState::RealThread`), never to the `Thread/
    // currentThread` main-stand-in (`HostState::Thread`) -- checked
    // FIRST, before the match below, so the two states can't be
    // confused (a `match` arm can't otherwise express "only if this
    // variant" without an irrefutable-pattern warning).
    // `Thread.isInterrupted` / `Thread/interrupted`-style test on the current thread: the
    // interpreter's interrupt flag (what an nREPL `interrupt` sets)
    if let ("isInterrupted", []) = (method, args) {
        if !matches!(&*crate::sync::lock_mutex(&h.state), HostState::RealThread(_)) {
            return Ok(Value::Bool(interp.intr.pending()));
        }
        return Ok(Value::Bool(false));
    }
    if let ("start" | "join", []) = (method, args) {
        let mut guard = crate::sync::lock_mutex(&h.state);
        let HostState::RealThread(rt) = &mut *guard else {
            return Err(unknown_method(HostKind::Thread, method, args.len(), span));
        };
        if method == "start" {
            if rt.started {
                // Measured: real `Thread.start()` on an already-started
                // thread throws `IllegalThreadStateException`. Not a
                // conformance-scored path (nothing in this task's corpus
                // double-starts), an honest error either way.
                return Err(RjError::other("Thread.start: thread already started").with_span(span));
            }
            rt.started = true;
            let f = rt.f.clone();
            let cell = rt.cell.clone();
            drop(guard);
            // Same shape as `builtins::conc`'s `future*` -- see that fn's
            // own doc for why each piece (fork, stack size,
            // detach-and-let-the-cell-outlive-the-JoinHandle) is what it
            // is; duplicated here (not reused as a shared helper)
            // because `future*`'s registration is a native-fn closure,
            // not a plain function this module could call into. ONE
            // deliberate difference from `future*`: NO binding
            // conveyance. Real `Thread.start()` runs the body with the
            // ROOT dynamic bindings (only `future`/`binding-conveyor-fn`
            // convey) -- measured consequence in the vendored suite:
            // `clojure.test` assertions inside a raw thread do not reach
            // the test-thread's bound `*report-counters*` (delays.clj's
            // oracle counts 25, not 3,000,025), while parallel.clj's
            // `future-fn-properly-retains-conveyed-bindings` needs
            // `future*`'s conveyance to stay.
            let builder = std::thread::Builder::new()
                .name("mova-thread".to_string())
                .stack_size(THREAD_STACK_SIZE);
            let mut forked = interp.fork();
            let thread_cell = cell.clone();
            let body = move || {
                let result = forked.call(&f, &[]);
                let new_state = match result {
                    Ok(v) => crate::value::FutureState::Done(v),
                    Err(e) => crate::value::FutureState::Failed(e),
                };
                // L3/W2b: `resolve_future`, not a bare store + `notify_all`
                // -- `.join` (and `deref`, same cell) can be waiting as a
                // TASK, whose waker lives in `cell.task_wakers` and would be
                // stranded by a resolver that only rang the condvar. See
                // `builtins::conc::future_deref`'s ordering proof.
                crate::builtins::conc::resolve_future(&thread_cell, new_state);
            };
            // **L5/W3 fence #3 (design §4): in sim, a TASK, not an OS
            // thread.** Same treatment and same reason as `future*` (see its
            // registration in `builtins::conc`): an in-flight OS thread is
            // invisible to the sim advance rule, which reads "no runnable
            // task" as quiescence and jumps virtual time straight past the
            // thread's work (P6b F2). The closure above is used VERBATIM by
            // both placements -- including the deliberate absence of binding
            // conveyance documented just above, which the task path therefore
            // preserves exactly (a `Thread.` body still sees the ROOT dynamic
            // bindings under sim, not the spawner's).
            if crate::clock::sim_enabled() {
                crate::runtime::spawn(body);
                return Ok(Value::Nil);
            }
            let spawn_result = builder.spawn(crate::memstat::drained(body));
            return match spawn_result {
                Ok(handle) => {
                    drop(handle);
                    Ok(Value::Nil)
                }
                Err(e) => Err(RjError::other(format!("Thread.start: couldn't spawn thread: {e}")).with_span(span)),
            };
        }
        // "join": a never-started thread is "not alive" on the real JVM,
        // so `.join()` on one returns immediately (measured semantics,
        // not exercised by this task's corpus, which always `.start`s
        // before anything could `.join`) -- checked before blocking so
        // this can't wait forever on a cell nothing will ever fill.
        if !rt.started {
            return Ok(Value::Nil);
        }
        let cell = rt.cell.clone();
        drop(guard);
        // `Thread.join()` does NOT re-throw the run() body's own
        // exception to the joiner (an uncaught exception in a Java
        // thread goes to its `UncaughtExceptionHandler`, never to
        // whoever `.join()`s it) -- so `f`'s `Err` is deliberately
        // swallowed here, matching that real asymmetry (unlike
        // `future_deref`'s OTHER callers, `deref`/`@`, which DO
        // re-propagate a future's failure to the deref-ing side).
        let _ = crate::builtins::conc::future_deref(&cell, None, None);
        return Ok(Value::Nil);
    }
    match (method, args) {
        ("getName", []) => Ok(Value::Str("main".into())),
        // S5: cheap, single-threaded stand-in -- the real JVM's main
        // thread id is also `1` on a fresh process, so this happens to be
        // exact, not just plausible.
        ("getId", []) => Ok(Value::Int(1)),
        // S5: real `getStackTrace()` returns `StackTraceElement[]`; mova
        // has no `StackTraceElement` class, so this is an empty
        // `Object`-kind array -- a documented coarse stub (see this
        // module's doc), good enough for `(count (.getStackTrace
        // (Thread/currentThread)))`-shaped code, not for reflecting over
        // real frames.
        ("getStackTrace", []) => Ok(Value::Array(Arc::new(ArrayVal {
            kind: ArrayKind::Object("java.lang.Object"),
            dims: 1,
            data: Mutex::new(Vec::new()),
        }))),
        _ => Err(unknown_method(HostKind::Thread, method, args.len(), span)),
    }
}

/// `(.await barrier)` -- `java.util.concurrent.CyclicBarrier`'s one
/// method this task's scope needs. The `Arc<std::sync::Barrier>` is
/// cloned OUT from under `h.state`'s mutex and the lock dropped BEFORE
/// calling `.wait()` on the clone: `.wait()` blocks until every party
/// arrives, so holding `h.state`'s lock across it would mean only ONE
/// thread could even be IN this method at a time, which defeats the
/// entire point of a barrier (every other party would deadlock trying to
/// lock `h.state` for their own `.await`). The `Barrier` itself is
/// `Send + Sync` with its own internal synchronization, so releasing this
/// module's lock first is sound, not just convenient.
///
/// Real `CyclicBarrier.await()` returns the calling thread's arrival
/// index (`parties - 1` for the first arrival, `0` for the last) --
/// `std::sync::Barrier::wait()` exposes no such index (only
/// `is_leader()`), and nothing in this task's corpus reads the return
/// value, so this returns `Value::Nil` rather than fabricate a number
/// nothing measures.
fn call_barrier_method(h: &Arc<HostInstVal>, method: &str, args: &[Value], span: Span) -> Result<Value, RjError> {
    match (method, args) {
        ("await", []) => {
            let barrier = {
                let guard = crate::sync::lock_mutex(&h.state);
                let HostState::CyclicBarrier(b) = &*guard else {
                    unreachable!("HostKind::CyclicBarrier cell always holds HostState::CyclicBarrier");
                };
                b.clone()
            };
            barrier.wait();
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(HostKind::CyclicBarrier, method, args.len(), span)),
    }
}

fn call_threadlocal_method(
    interp: &mut Interp,
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    match (method, args) {
        ("get", []) => {
            let existing = {
                let guard = crate::sync::lock_mutex(&h.state);
                let HostState::ThreadLocal(tl) = &*guard else {
                    unreachable!("HostKind::ThreadLocal cell always holds HostState::ThreadLocal")
                };
                tl.value.clone()
            };
            if let Some(v) = existing {
                return Ok(v);
            }
            let init_fn = {
                let guard = crate::sync::lock_mutex(&h.state);
                let HostState::ThreadLocal(tl) = &*guard else {
                    unreachable!("checked above")
                };
                tl.init_fn.clone()
            };
            // Lock released before calling into the interpreter: `init_fn`
            // is arbitrary user code (`interp.call`), which must never run
            // while `h.state`'s mutex is held.
            let computed = match init_fn {
                Some(f) => interp.call(&f, &[])?,
                None => Value::Nil,
            };
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::ThreadLocal(tl) = &mut *guard else {
                unreachable!("checked above")
            };
            tl.value = Some(computed.clone());
            Ok(computed)
        }
        ("set", [v]) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::ThreadLocal(tl) = &mut *guard else {
                unreachable!("HostKind::ThreadLocal cell always holds HostState::ThreadLocal")
            };
            tl.value = Some(v.clone());
            Ok(Value::Nil)
        }
        ("remove", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::ThreadLocal(tl) = &mut *guard else {
                unreachable!("HostKind::ThreadLocal cell always holds HostState::ThreadLocal")
            };
            tl.value = None;
            Ok(Value::Nil)
        }
        _ => Err(unknown_method(
            HostKind::ThreadLocal,
            method,
            args.len(),
            span,
        )),
    }
}

/// `Thread/currentThread` -- registered by `builtins::hostclass::register`
/// as the qualified global `Symbol { ns: Some("Thread"), name:
/// "currentThread" }`, the exact candidate `ns::for_each_global_candidate`
/// probes first for a `Thread/currentThread` call site (see that fn's
/// doc). A fresh stand-in `Thread` instance every call (matches real
/// Clojure's own `(= (Thread/currentThread) (Thread/currentThread))`
/// being `true` there ONLY because both calls return the SAME real
/// `Thread` object by reference identity -- mova's stand-in does not
/// preserve that identity across calls, a documented, narrow divergence:
/// nothing in this task's scope compares two `Thread/currentThread`
/// results for `=`).
pub(crate) fn current_thread(_interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    Ok(mk_thread())
}

/// Registers `Thread/currentThread` -- the ONE static call site this task
/// needs (see this module's doc). Not folded into `builtins::types::
/// install`'s `builtin_classes` loop (that loop only binds CLASS values,
/// never natives) -- called directly from `builtins::register_core`
/// instead, alongside `types::install`.
pub fn register(i: &mut Interp) {
    i.globals.set_builtin(
        crate::value::Symbol {
            ns: Some("Thread".into()),
            name: "currentThread".into(),
        },
        Value::Native(Arc::new(crate::value::NativeFn::new(
            "Thread/currentThread",
            current_thread,
        ))),
    );

    // W4-veneer: `java.nio.file.Files/newBufferedReader` -- see that fn's
    // own doc.
    i.globals.set_builtin(
        crate::value::Symbol { ns: Some("java.nio.file.Files".into()), name: "newBufferedReader".into() },
        Value::Native(Arc::new(crate::value::NativeFn::new(
            "java.nio.file.Files/newBufferedReader",
            files_new_buffered_reader,
        ))),
    );

    // W4-veneer: `ReflectorTryCatchFixture/fail` -- see `reflector_
    // fixture_fail`'s own doc. Registered under the CANONICAL (fully-
    // qualified) name -- `(:import [clojure.test ReflectorTryCatchFixture
    // ...])` binds the short alias to this same qualified name via
    // `ns::expand_alias` (same S7 `UUID/randomUUID` mechanism that fn's
    // own doc describes), so `(ReflectorTryCatchFixture/fail x)` resolves
    // here with no separate short-name registration needed.
    i.globals.set_builtin(
        crate::value::Symbol {
            ns: Some("clojure.test.ReflectorTryCatchFixture".into()),
            name: "fail".into(),
        },
        Value::Native(Arc::new(crate::value::NativeFn::new(
            "ReflectorTryCatchFixture/fail",
            reflector_fixture_fail,
        ))),
    );

    // S6/libstatics: the five exception/`Throwable` class VARS -- same
    // "every alias spelling binds the same interned class value" shape
    // `builtins::types::install`'s `builtin_classes` loop uses (bare
    // short name + fully-qualified name both resolve to the identical
    // `Value::Class`, so `(= IllegalArgumentException
    // java.lang.IllegalArgumentException)` holds), registered directly
    // here rather than added to that owned table -- see this arm
    // group's doc above `exception_ctor` for why. `Throwable` itself has
    // no constructor arm in `construct` (nothing in this task's scope
    // constructs one directly), but still needs a class VAR so
    // `(instance? Throwable x)` (measured: `clojure.test.check.
    // properties`'s own failure-detection idiom) resolves at all.
    //
    // W4-veneer: factored into `exception_class_rows()` below (was an
    // inline `let` here) so `types::class_of`'s `Value::Inst` arm can
    // consult the SAME table -- see that fn's own doc for why (a
    // `mk_exception`-built instance's `(type e)`/`(class e)` used to mint
    // a FRESH `ClassVal::User` every time rather than resolving to the
    // canonical `ClassVal::Builtin` these rows bind as a global var,
    // making `(= java.io.FileNotFoundException (type e))` false even
    // though both print identically).
    for (aliases, canonical, pred) in exception_class_rows() {
        let cv = Value::Class(Arc::new(crate::types::ClassVal::Builtin {
            name: canonical,
            pred: Some(*pred),
        }));
        for alias in *aliases {
            i.globals.set(crate::value::Symbol::simple(*alias), cv.clone());
        }
    }
}

/// The exception/`Throwable` class rows `register` binds as global vars --
/// see that fn's own doc for why they live here rather than in
/// `types::builtin_classes()`, and `types::class_of`'s doc for the OTHER
/// consumer (looking these up by name, not by binding a var).
pub(crate) fn exception_class_rows() -> &'static [(&'static [&'static str], &'static str, fn(&Value) -> bool)]
{
    &[
        (
            &["IllegalArgumentException", "java.lang.IllegalArgumentException"],
            "java.lang.IllegalArgumentException",
            pred_illegal_argument_exception,
        ),
        (
            &["IndexOutOfBoundsException", "java.lang.IndexOutOfBoundsException"],
            "java.lang.IndexOutOfBoundsException",
            pred_index_out_of_bounds_exception,
        ),
        (
            &["RuntimeException", "java.lang.RuntimeException"],
            "java.lang.RuntimeException",
            pred_runtime_exception,
        ),
        // mova/PLAN.md interop-census batch: bare AND fully-qualified,
        // same auto-import reasoning as the other java.lang.* rows.
        (
            &["ArithmeticException", "java.lang.ArithmeticException"],
            "java.lang.ArithmeticException",
            pred_arithmetic_exception,
        ),
        (
            &["ClassCastException", "java.lang.ClassCastException"],
            "java.lang.ClassCastException",
            pred_class_cast_exception,
        ),
        (
            &["NullPointerException", "java.lang.NullPointerException"],
            "java.lang.NullPointerException",
            pred_null_pointer_exception,
        ),
        (
            &["NumberFormatException", "java.lang.NumberFormatException"],
            "java.lang.NumberFormatException",
            pred_number_format_exception,
        ),
        (
            &["IllegalStateException", "java.lang.IllegalStateException"],
            "java.lang.IllegalStateException",
            pred_illegal_state_exception,
        ),
        (
            &[
                "UnsupportedOperationException",
                "java.lang.UnsupportedOperationException",
            ],
            "java.lang.UnsupportedOperationException",
            pred_unsupported_operation_exception,
        ),
        (
            &["Exception", "java.lang.Exception"],
            "java.lang.Exception",
            pred_exception_class,
        ),
        (
            &["Throwable", "java.lang.Throwable"],
            "java.lang.Throwable",
            pred_throwable,
        ),
        // S6 (assert/namespace/uuid batch): `AssertionError`/`Error` join
        // the table for `core.mova`'s new `assert` macro (see
        // `construct`'s `"java.lang.AssertionError"` arm and
        // `ASSERTION_ERROR_ANCESTORS` for why `Error` needs its own row
        // rather than reusing `Throwable`'s pred).
        (
            &["AssertionError", "java.lang.AssertionError"],
            "java.lang.AssertionError",
            pred_assertion_error,
        ),
        (&["Error", "java.lang.Error"], "java.lang.Error", pred_error),
        (&["StackOverflowError", "java.lang.StackOverflowError"], "java.lang.StackOverflowError", pred_stack_overflow_error),
        // W4-veneer (try_catch.clj): `java.io.FileNotFoundException` --
        // fully-qualified only, like `java.net.URI` below (`java.io.*` is
        // not `java.lang.*`, no bare alias auto-resolves on the real JVM
        // either, and the vendored `are` table names it fully-qualified).
        (
            &["java.io.FileNotFoundException"],
            "java.io.FileNotFoundException",
            pred_file_not_found_exception,
        ),
        // S6 (uuid/uri predicates batch): `java.net.URI` -- ONLY the
        // fully-qualified spelling (measured: like `java.util.Random`/
        // `java.util.Date` above, `URI` alone is not `java.lang.*` and
        // does not auto-resolve on the real JVM either, so no short
        // alias is added).
        (&["java.net.URI"], "java.net.URI", pred_uri),
        // C3c (errors.clj's `arity-exception` deftest): `clojure.lang.
        // ArityException` -- bare AND fully-qualified (real Clojure
        // auto-imports every `clojure.lang.*` class bare, same as
        // `RuntimeException`/`Exception`/... above). `catch`'s own class
        // symbol is never evaluated (`eval::special_forms::
        // parse_catch_head`'s doc), so this row exists for `instance?`/
        // `class` consistency, not to make the `catch ArityException e`
        // clause itself work -- that already worked with no class
        // resolution at all.
        (
            &["ArityException", "clojure.lang.ArityException"],
            "clojure.lang.ArityException",
            pred_arity_exception,
        ),
        // W4-EVAL task 4: bare AND fully-qualified, same "real Clojure
        // auto-imports every java.lang.* class bare" reasoning as
        // `RuntimeException`/`Exception`/... above -- needed so
        // `(IllegalAccessError. "msg")` (`core.mova`'s `check-transient!`)
        // resolves a class to construct against in the first place.
        // (Merge note: the branch added this against the pre-W4-VENEER
        // inline table; entry unioned into `exception_class_rows()`.)
        (
            &["IllegalAccessError", "java.lang.IllegalAccessError"],
            "java.lang.IllegalAccessError",
            pred_illegal_access_error,
        ),
    ]
}

/// lsp/kondo: `java.util.StringTokenizer`'s method surface -- `.
/// hasMoreTokens`/`.nextToken`/`.nextToken(newDelim)`/`.countTokens`
/// (the `Enumeration` methods `.hasMoreElements`/`.nextElement` are the
/// exact same operations under a different name, real JDK's
/// `StringTokenizer` implements both interfaces over one algorithm).
/// `.nextToken` on an exhausted tokenizer throws real JDK's
/// `NoSuchElementException` (`JvmClass::NoSuchElement`).
fn call_string_tokenizer_method(
    h: &Arc<HostInstVal>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RjError> {
    // Real JDK `skipDelimiters`: when NOT returning delimiters, advance
    // past every leading delimiter char first (consecutive/leading
    // delimiters never produce empty tokens this way); when returning
    // delimiters, `pos` is used as-is -- every remaining char, delimiter
    // or not, starts its own token.
    fn skip_delims(chars: &[char], mut pos: usize, delims: &str, return_delims: bool) -> usize {
        if !return_delims {
            while pos < chars.len() && delims.contains(chars[pos]) {
                pos += 1;
            }
        }
        pos
    }
    // Real JDK `scanToken`: from a (possibly skipped) start, either take
    // ONE char (it's a delimiter and we're returning delimiters -- each
    // delimiter is its own one-char token, never grouped into a run), or
    // take the whole run up to the next delimiter/end.
    fn scan_token(chars: &[char], start: usize, delims: &str, return_delims: bool) -> (Str, usize) {
        if return_delims && delims.contains(chars[start]) {
            return (Str::from(chars[start].to_string()), start + 1);
        }
        let mut end = start;
        while end < chars.len() && !delims.contains(chars[end]) {
            end += 1;
        }
        (Str::from(chars[start..end].iter().collect::<String>()), end)
    }

    match (method, args) {
        ("hasMoreTokens" | "hasMoreElements", []) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::StringTokenizer { chars, pos, delims, return_delims } = &*guard else {
                unreachable!("HostKind::StringTokenizer always carries HostState::StringTokenizer")
            };
            let skipped = skip_delims(chars, *pos, delims, *return_delims);
            Ok(Value::Bool(skipped < chars.len()))
        }
        ("countTokens", []) => {
            let guard = crate::sync::lock_mutex(&h.state);
            let HostState::StringTokenizer { chars, pos, delims, return_delims } = &*guard else {
                unreachable!("HostKind::StringTokenizer always carries HostState::StringTokenizer")
            };
            let mut cursor = *pos;
            let mut count = 0i64;
            loop {
                cursor = skip_delims(chars, cursor, delims, *return_delims);
                if cursor >= chars.len() {
                    break;
                }
                let (_, next) = scan_token(chars, cursor, delims, *return_delims);
                cursor = next;
                count += 1;
            }
            Ok(Value::Int(count))
        }
        ("nextToken" | "nextElement", []) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::StringTokenizer { chars, pos, delims, return_delims } = &mut *guard else {
                unreachable!("HostKind::StringTokenizer always carries HostState::StringTokenizer")
            };
            let start = skip_delims(chars, *pos, delims, *return_delims);
            if start >= chars.len() {
                return Err(RjError::other("java.util.StringTokenizer: no more tokens (NoSuchElementException)")
                    .with_class(JvmClass::NoSuchElement)
                    .with_span(span));
            }
            let (tok, next) = scan_token(chars, start, delims, *return_delims);
            *pos = next;
            Ok(Value::Str(tok))
        }
        // Real `nextToken(String newDelim)`: swaps the delimiter set
        // (persists for every later call, including plain `.nextToken`)
        // and continues from the CURRENT position, not from scratch.
        ("nextToken", [Value::Str(new_delims)]) => {
            let mut guard = crate::sync::lock_mutex(&h.state);
            let HostState::StringTokenizer { chars, pos, delims, return_delims } = &mut *guard else {
                unreachable!("HostKind::StringTokenizer always carries HostState::StringTokenizer")
            };
            *delims = new_delims.clone();
            let start = skip_delims(chars, *pos, delims, *return_delims);
            if start >= chars.len() {
                return Err(RjError::other("java.util.StringTokenizer: no more tokens (NoSuchElementException)")
                    .with_class(JvmClass::NoSuchElement)
                    .with_span(span));
            }
            let (tok, next) = scan_token(chars, start, delims, *return_delims);
            *pos = next;
            Ok(Value::Str(tok))
        }
        _ => Err(unknown_method(h.kind, method, args.len(), span)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mova/PLAN.md interop-census batch: `IllegalStateException`/
    /// `UnsupportedOperationException` ctors -- same 0/1/2-arg shape as
    /// the other `exception_ctor`-backed rows (see that fn's doc).
    #[test]
    fn illegal_state_and_unsupported_operation_ctors_build_named_exceptions() {
        let span = Span { start: 0, end: 0 };
        let ise = construct(
            &mut Interp::new(),
            "java.lang.IllegalStateException",
            &[Value::Str("bad state".to_string().into())],
            span,
        )
        .unwrap();
        assert!(pred_illegal_state_exception(&ise));
        assert!(!pred_unsupported_operation_exception(&ise));

        let uoe = construct(
            &mut Interp::new(),
            "java.lang.UnsupportedOperationException",
            &[],
            span,
        )
        .unwrap();
        assert!(pred_unsupported_operation_exception(&uoe));
    }

    /// Measured against real Clojure 1.13.0-alpha6 (`.oracle`), seed 42:
    /// `(let [r (java.util.Random. 42)] [(.nextInt r) (.nextInt r 100)
    /// (.nextLong r) (.nextDouble r) (.nextFloat r) (.nextBoolean r)])`
    /// -> `[-1170105035 63 -5843495416241995736 0.30871945533265976
    /// 0.27707845 true]`. See `tests/conformance/corpus/hostclasses.corpus`
    /// for the same transcript run through the full corpus harness, plus a
    /// second seed.
    #[test]
    fn random_seed_42_matches_real_jvm() {
        let mut seed = scramble(42);
        assert_eq!(next_int(&mut seed), -1170105035);
        assert_eq!(
            next_int_bound(&mut seed, 100, Span { start: 0, end: 0 }).unwrap(),
            63
        );
        assert_eq!(next_long(&mut seed), -5843495416241995736);
        assert_eq!(next_double(&mut seed), 0.30871945533265976);
        assert_eq!(next_float(&mut seed), 0.27707845f32 as f64);
        assert!(next_boolean(&mut seed));
    }

    /// Same shape, seed 0 -- an independent second data point (also in
    /// `hostclasses.corpus`). `(let [r (java.util.Random. 0)] [(.nextInt r)
    /// (.nextInt r) (.nextInt r 10)])` -> `[-1155484576 -723955400 9]`.
    #[test]
    fn random_seed_0_matches_real_jvm() {
        let mut seed = scramble(0);
        assert_eq!(next_int(&mut seed), -1155484576);
        assert_eq!(next_int(&mut seed), -723955400);
        assert_eq!(
            next_int_bound(&mut seed, 10, Span { start: 0, end: 0 }).unwrap(),
            9
        );
    }

    /// `(let [r (java.util.Random. 42)] (repeatedly 5 #(.nextInt r)))` ->
    /// `(-1170105035 234785527 -1360544799 205897768 1325939940)` --
    /// repeated `.nextInt` calls on the SAME instance (mutation, not a
    /// fresh seed each time).
    #[test]
    fn random_repeated_next_int_matches_real_jvm() {
        let mut seed = scramble(42);
        let want = [
            -1170105035i32,
            234785527,
            -1360544799,
            205897768,
            1325939940,
        ];
        for w in want {
            assert_eq!(next_int(&mut seed), w);
        }
    }

    /// `(let [r (java.util.Random. 12345)] [(.nextInt r 7) (.nextInt r 7)
    /// (.nextInt r 7) (.nextLong r) (.nextDouble r)])` -> `[5 2 4
    /// -1528963862231680626 0.03767297158354166]` -- a third seed,
    /// exercising `nextInt(bound)`'s rejection-loop branch (7 is not a
    /// power of two) three times in a row on one instance.
    #[test]
    fn random_seed_12345_matches_real_jvm() {
        let span = Span { start: 0, end: 0 };
        let mut seed = scramble(12345);
        assert_eq!(next_int_bound(&mut seed, 7, span).unwrap(), 5);
        assert_eq!(next_int_bound(&mut seed, 7, span).unwrap(), 2);
        assert_eq!(next_int_bound(&mut seed, 7, span).unwrap(), 4);
        assert_eq!(next_long(&mut seed), -1528963862231680626);
        assert_eq!(next_double(&mut seed), 0.03767297158354166);
    }

    /// lsp/io: `clojure.java.io`'s `java.io.File` veneer methods --
    /// `exists`/`isDirectory`/`isFile`/`getName`/`getParentFile`/
    /// `mkdirs`/`listFiles` -- against a real tempdir, matching the JVM's
    /// own `java.io.File` semantics (no mocking of the filesystem).
    #[test]
    fn javafile_methods_match_real_filesystem() {
        let span = Span { start: 0, end: 0 };
        let dir = std::env::temp_dir().join(format!("mova-javafile-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("a.txt");
        std::fs::write(&file_path, b"hi").unwrap();

        let dir_h = match mk_java_file(Str::from(dir.to_string_lossy().into_owned())) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert_eq!(
            call_javafile_method(&dir_h, "exists", &[], span).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            call_javafile_method(&dir_h, "isDirectory", &[], span).unwrap(),
            Value::Bool(true)
        );
        let Value::Vector(listed) = call_javafile_method(&dir_h, "listFiles", &[], span).unwrap() else {
            panic!("expected a vector")
        };
        assert_eq!(listed.len(), 1);

        let file_h = match mk_java_file(Str::from(file_path.to_string_lossy().into_owned())) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert_eq!(
            call_javafile_method(&file_h, "isFile", &[], span).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            call_javafile_method(&file_h, "getName", &[], span).unwrap(),
            Value::Str(Str::from("a.txt".to_string()))
        );
        // lsp/io: `.length` -- real `File.length()` returns the size in
        // bytes (`"hi"` is 2), or `0` for a file that doesn't exist
        // (never throws). Regression coverage for the clojure-lsp
        // integration bug where clj-kondo's own single-file lint path
        // called `.length` on a real `java.io.File` and, missing this
        // arm, threw "no method .length" -- silently swallowed by
        // clojure-lsp's `try/catch`, which dropped `publishDiagnostics`
        // for every freshly-opened file with zero diagnostics.
        assert_eq!(call_javafile_method(&file_h, "length", &[], span).unwrap(), Value::Int(2));
        let Value::Str(parent_name) =
            call_javafile_method(&file_h, "getParentFile", &[], span).and_then(|v| {
                let Value::HostInst(ph) = v else { panic!("expected a File") };
                call_javafile_method(&ph, "getName", &[], span)
            }).unwrap()
        else {
            panic!("expected a string")
        };
        assert_eq!(parent_name.as_ref(), dir.file_name().unwrap().to_str().unwrap());

        let missing = dir.join("nope");
        let missing_h = match mk_java_file(Str::from(missing.to_string_lossy().into_owned())) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert_eq!(
            call_javafile_method(&missing_h, "exists", &[], span).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(call_javafile_method(&missing_h, "length", &[], span).unwrap(), Value::Int(0));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// lsp/io (review round 2): `clojure.java.io/writer`'s new real
    /// streaming output stream -- `.write`/`.append` go straight through
    /// to the file (not buffered-then-written-whole on `.close`), and
    /// `.close` flushes.
    #[test]
    fn output_stream_over_a_file_streams_writes_through() {
        let span = Span { start: 0, end: 0 };
        let path = std::env::temp_dir().join(format!("mova-outputstream-test-{}.txt", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        let w = match mk_output_stream(Box::new(f)) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        call_output_stream_method(&w, "write", &[Value::Str(Str::from("hello ".to_string()))], span).unwrap();
        call_output_stream_method(&w, "append", &[Value::Str(Str::from("world".to_string()))], span).unwrap();
        // Not flushed/closed yet -- BufWriter may still be holding the
        // bytes; this proves the write path is real streaming (the file
        // handle is the SAME one, no in-memory accumulate-then-write-
        // whole-file step) rather than asserting anything about timing.
        call_output_stream_method(&w, "close", &[], span).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello world");
        std::fs::remove_file(&path).ok();
    }

    /// lsp/io: an input stream over an in-memory byte/string source --
    /// `.readLine`/`mova.io/read-bytes`'s underlying helper, EOF as
    /// `None`, and a `Content-Length`-shaped read.
    #[test]
    fn input_stream_over_string_content_reads_lines_and_exact_byte_counts() {
        let content = "Content-Length: 5\r\n\r\nhello";
        let r = match mk_input_stream(Box::new(std::io::Cursor::new(content.as_bytes().to_vec()))) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert_eq!(stream_read_line(&r).unwrap(), Some(Str::from("Content-Length: 5".to_string())));
        assert_eq!(stream_read_line(&r).unwrap(), Some(Str::from(String::new())));
        assert_eq!(stream_read_n_bytes(&r, 5).unwrap(), Some(Str::from("hello".to_string())));
        // Clean EOF now -- no more bytes at all.
        assert_eq!(stream_read_n_bytes(&r, 1).unwrap(), None);
        assert_eq!(stream_read_line(&r).unwrap(), None);
    }

    /// lsp/io: a partial read that hits EOF mid-frame is a real error,
    /// not a silent truncation or a clean `None` -- matches the JVM
    /// original's `EOFException`.
    #[test]
    fn input_stream_partial_read_then_eof_is_an_error() {
        let r = match mk_input_stream(Box::new(std::io::Cursor::new(b"abc".to_vec()))) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert!(stream_read_n_bytes(&r, 10).is_err());
    }

    /// lsp/io: `.close` on an input stream releases the underlying
    /// source right away (a further read hits EOF), not whenever the
    /// `Arc` happens to be dropped -- `with-open`'s whole point.
    #[test]
    fn close_makes_further_reads_see_eof() {
        let span = Span { start: 0, end: 0 };
        let r = match mk_input_stream(Box::new(std::io::Cursor::new(b"abc".to_vec()))) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        call_input_stream_method(&r, "close", &[], span).unwrap();
        // a closed stream fails every read, as on the JVM ("Stream closed")
        let e = stream_read_byte(&r).unwrap_err();
        assert_eq!(e.message, "Stream closed");
    }

    /// kondo-wave: `ReentrantLock` is a REAL mutex now, not a no-op --
    /// same-thread `.lock` is reentrant (count-based); a DIFFERENT
    /// thread's `.lock` genuinely blocks until every reentrant hold is
    /// `.unlock`ed; `.unlock` by a non-owner errors (real Java throws
    /// `IllegalMonitorStateException` there too).
    #[test]
    fn reentrant_lock_is_real() {
        let span = Span { start: 0, end: 0 };
        let h = Arc::new(HostInstVal {
            kind: HostKind::ReentrantLock,
            state: Mutex::new(HostState::ReentrantLock(Arc::new((
                Mutex::new(ReentrantLockState::default()),
                Condvar::new(),
            )))),
        });
        // same thread: lock is reentrant (2 holds total after this).
        call_reentrant_lock_method(&h, "lock", &[], span).unwrap();
        call_reentrant_lock_method(&h, "lock", &[], span).unwrap();
        assert_eq!(
            call_reentrant_lock_method(&h, "isLocked", &[], span).unwrap(),
            Value::Bool(true)
        );

        let h2 = h.clone();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started2, acquired2) = (started.clone(), acquired.clone());
        let other = std::thread::spawn(move || {
            started2.store(true, Ordering::SeqCst);
            call_reentrant_lock_method(&h2, "lock", &[], span).unwrap();
            acquired2.store(true, Ordering::SeqCst);
            call_reentrant_lock_method(&h2, "unlock", &[], span).unwrap();
        });
        while !started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!acquired.load(Ordering::SeqCst), "other thread must block while we hold the lock");

        // drop both of our holds -- only now may the other thread proceed.
        call_reentrant_lock_method(&h, "unlock", &[], span).unwrap();
        call_reentrant_lock_method(&h, "unlock", &[], span).unwrap();
        other.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));

        // unlock by a non-owner (nobody holds it now) errors.
        assert!(call_reentrant_lock_method(&h, "unlock", &[], span).is_err());
    }

    /// lsp/kondo: real JDK `StringTokenizer` semantics -- empty tokens
    /// skipped (consecutive/leading/trailing delimiters), `returnDelims`
    /// makes each delimiter its own one-char token, `countTokens` doesn't
    /// consume, and an exhausted `.nextToken` throws `NoSuchElementException`.
    #[test]
    fn stringtokenizer_semantics() {
        let span = Span { start: 0, end: 0 };

        // Default delimiters (whitespace): consecutive/leading/trailing
        // spaces never produce empty tokens.
        let h = match mk_string_tokenizer(Str::from("  a b  c "), Str::from(DEFAULT_TOKENIZER_DELIMS), false) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert_eq!(call_string_tokenizer_method(&h, "countTokens", &[], span).unwrap(), Value::Int(3));
        // countTokens must not have consumed anything.
        assert_eq!(call_string_tokenizer_method(&h, "hasMoreTokens", &[], span).unwrap(), Value::Bool(true));
        assert_eq!(call_string_tokenizer_method(&h, "nextToken", &[], span).unwrap(), Value::Str(Str::from("a")));
        assert_eq!(call_string_tokenizer_method(&h, "nextElement", &[], span).unwrap(), Value::Str(Str::from("b")));
        assert_eq!(call_string_tokenizer_method(&h, "nextToken", &[], span).unwrap(), Value::Str(Str::from("c")));
        assert_eq!(call_string_tokenizer_method(&h, "hasMoreElements", &[], span).unwrap(), Value::Bool(false));
        // exhausted -- NoSuchElementException, not a generic error.
        let err = call_string_tokenizer_method(&h, "nextToken", &[], span).unwrap_err();
        assert_eq!(err.jvm_class, Some(JvmClass::NoSuchElement));

        // Custom delimiters, no returnDelims: "a,,b,c" over "," skips the
        // empty token between the two commas.
        let h2 = match mk_string_tokenizer(Str::from("a,,b,c"), Str::from(","), false) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        let mut toks = Vec::new();
        while call_string_tokenizer_method(&h2, "hasMoreTokens", &[], span).unwrap() == Value::Bool(true) {
            toks.push(call_string_tokenizer_method(&h2, "nextToken", &[], span).unwrap());
        }
        assert_eq!(toks, vec![Value::Str(Str::from("a")), Value::Str(Str::from("b")), Value::Str(Str::from("c"))]);

        // returnDelims: each delimiter char is its OWN token, runs of
        // non-delimiter chars stay grouped.
        let h3 = match mk_string_tokenizer(Str::from("a,,b"), Str::from(","), true) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert_eq!(call_string_tokenizer_method(&h3, "countTokens", &[], span).unwrap(), Value::Int(4));
        let mut toks3 = Vec::new();
        for _ in 0..4 {
            toks3.push(call_string_tokenizer_method(&h3, "nextToken", &[], span).unwrap());
        }
        assert_eq!(
            toks3,
            vec![
                Value::Str(Str::from("a")),
                Value::Str(Str::from(",")),
                Value::Str(Str::from(",")),
                Value::Str(Str::from("b")),
            ]
        );

        // `nextToken(newDelim)` swaps the delimiter set mid-stream and
        // continues from the CURRENT position (not from scratch): after
        // "a" is consumed on ":", switching to " " scans from right
        // after "a" using the NEW delimiter, so ":b" (the ":" is no
        // longer a delimiter) comes back as one token.
        let h4 = match mk_string_tokenizer(Str::from("a:b c"), Str::from(":"), false) {
            Value::HostInst(h) => h,
            _ => unreachable!(),
        };
        assert_eq!(call_string_tokenizer_method(&h4, "nextToken", &[], span).unwrap(), Value::Str(Str::from("a")));
        assert_eq!(
            call_string_tokenizer_method(&h4, "nextToken", &[Value::Str(Str::from(" "))], span).unwrap(),
            Value::Str(Str::from(":b"))
        );
        assert_eq!(call_string_tokenizer_method(&h4, "nextToken", &[], span).unwrap(), Value::Str(Str::from("c")));
    }

    /// lsp/kondo: `JarFile`/`ZipFile` veneer -- one test per method
    /// (`.entries`/`enumeration-seq`'s hasMoreElements+nextElement,
    /// `.getEntry`/`.getJarEntry`, entry `.getName`/`.isDirectory`/
    /// `.getSize`, `.getInputStream`, `.close`), against a real jar
    /// built on disk with `zip::ZipWriter` (not hand-rolled bytes).
    #[test]
    fn jar_file_veneer_covers_every_method() {
        let span = Span { start: 0, end: 0 };
        let path = std::env::temp_dir().join(format!("mova-jar-veneer-test-{}.jar", std::process::id()));
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            zw.start_file("a/", zip::write::FileOptions::default()).unwrap();
            zw.start_file("a/hello.txt", zip::write::FileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zw, b"hello jar").unwrap();
            zw.finish().unwrap();
        }
        let path_str = path.to_str().unwrap().to_string();

        let jar = match construct(&mut Interp::new(), "java.util.jar.JarFile", &[Value::Str(Str::from(path_str))], span)
        {
            Ok(Value::HostInst(h)) => h,
            other => panic!("expected HostInst, got {other:?}"),
        };

        // .getEntry / .getJarEntry
        let entry = match call_jar_file_method(&jar, "getEntry", &[Value::Str(Str::from("a/hello.txt"))], span) {
            Ok(Value::HostInst(h)) => h,
            other => panic!("expected HostInst entry, got {other:?}"),
        };
        assert_eq!(call_jar_file_method(&jar, "getJarEntry", &[Value::Str(Str::from("nope"))], span).unwrap(), Value::Nil);

        // entry .getName / .isDirectory / .getSize
        assert_eq!(call_jar_entry_method(&entry, "getName", &[], span).unwrap(), Value::Str(Str::from("a/hello.txt")));
        assert_eq!(call_jar_entry_method(&entry, "isDirectory", &[], span).unwrap(), Value::Bool(false));
        assert_eq!(call_jar_entry_method(&entry, "getSize", &[], span).unwrap(), Value::Int(9));

        // .entries -> Enumeration: hasMoreElements/nextElement walk both entries
        let enu = match call_jar_file_method(&jar, "entries", &[], span) {
            Ok(Value::HostInst(h)) => h,
            other => panic!("expected HostInst enumeration, got {other:?}"),
        };
        let mut names = Vec::new();
        while call_jar_entries_method(&enu, "hasMoreElements", &[], span).unwrap() == Value::Bool(true) {
            let Value::HostInst(e) = call_jar_entries_method(&enu, "nextElement", &[], span).unwrap() else {
                panic!("nextElement must return a HostInst")
            };
            names.push(call_jar_entry_method(&e, "getName", &[], span).unwrap());
        }
        assert_eq!(names, vec![Value::Str(Str::from("a/")), Value::Str(Str::from("a/hello.txt"))]);
        assert!(call_jar_entries_method(&enu, "nextElement", &[], span).is_err());

        // .getInputStream reads the entry's bytes (slurp-able InputStream)
        let stream = call_jar_file_method(&jar, "getInputStream", &[Value::HostInst(entry.clone())], span).unwrap();
        let Value::HostInst(sh) = stream else { panic!("expected an InputStream HostInst") };
        assert_eq!(stream_read_all(&sh).unwrap(), Str::from("hello jar"));

        // .close gates further use
        call_jar_file_method(&jar, "close", &[], span).unwrap();
        assert!(call_jar_file_method(&jar, "entries", &[], span).is_err());

        let _ = std::fs::remove_file(&path);
    }
}
