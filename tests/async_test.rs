//! Integration tests for v0.2 / A2's core.async surface: `builtins::async`
//! (`chan`, `dropping-buffer`/`sliding-buffer`, `>!!`/`<!!`, `close!`,
//! `timeout`, `put!`/`take!`, `alts!!`/`offer!`/`poll!`, `go*`) plus the
//! `core/async.mova` macros (`go`, `go-loop`, `thread`, `>!`/`<!`, `alts!`,
//! `onto-chan!`).
//!
//! Timing-sensitive assertions use generous margins (matching
//! `tests/conc_test.rs`'s convention) against `time-ms`/short `sleep-ms`-
//! scale waits so they're not flaky on a loaded CI box.

use mova::embed::{Engine, Value, ValueKind};

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval_ok(src: &str) -> Value {
    engine()
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn eval_err(src: &str) -> String {
    match engine().eval_named("test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

fn as_int(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("expected an int, got {v:?}"))
}

// -------------------- unbuffered rendezvous --------------------

#[test]
fn unbuffered_rendezvous_go_puts_main_takes() {
    assert_eq!(
        ps(r#"(let [ch (chan) g (go (>! ch :hello) :sent)] (let [v (<!! ch)] (<!! g) v))"#),
        ":hello"
    );
}

// -------------------- fixed buffer backpressure --------------------

/// Two puts into a `(chan 2)` succeed without a consumer; a third blocks
/// until a take frees a slot.
#[test]
fn fixed_buffer_accepts_n_puts_then_blocks_until_a_take_frees_a_slot() {
    assert_eq!(
        ps(
            r#"(let [ch (chan 2)
                     _ (>!! ch :a)
                     _ (>!! ch :b)
                     f (future (>!! ch :c))
                     still-blocked (= :not-yet (deref f 50 :not-yet))
                     _ (<!! ch)
                     landed (deref f 500 :never)]
                 [still-blocked landed])"#
        ),
        "[true true]"
    );
}

// -------------------- dropping / sliding buffers --------------------

#[test]
fn dropping_buffer_keeps_first_n() {
    assert_eq!(
        ps(
            r#"(let [ch (chan (dropping-buffer 2))]
                 (>!! ch 1) (>!! ch 2) (>!! ch 3) (>!! ch 4)
                 (close! ch)
                 [(<!! ch) (<!! ch) (<!! ch)])"#
        ),
        "[1 2 nil]"
    );
}

#[test]
fn sliding_buffer_keeps_last_n() {
    assert_eq!(
        ps(
            r#"(let [ch (chan (sliding-buffer 2))]
                 (>!! ch 1) (>!! ch 2) (>!! ch 3) (>!! ch 4)
                 (close! ch)
                 [(<!! ch) (<!! ch) (<!! ch)])"#
        ),
        "[3 4 nil]"
    );
}

// -------------------- close! semantics --------------------

/// Combined: a put after close returns `false`, a closed-but-nonempty
/// channel drains its buffer before takes settle to `nil`, and a second
/// `close!` is an idempotent no-op (doesn't error, doesn't un-close).
#[test]
fn close_put_false_take_drains_then_nil_idempotent() {
    assert_eq!(
        ps(
            r#"(let [ch (chan 1)]
                 (>!! ch :x)
                 (close! ch)
                 (close! ch)
                 [(>!! ch :y) (<!! ch) (<!! ch)])"#
        ),
        "[false :x nil]"
    );
}

// -------------------- timeout --------------------

#[test]
fn timeout_channel_closes_after_roughly_ms() {
    let result = eval_ok(
        r#"(let [t0 (time-ms)
                 ch (timeout 50)
                 v (<!! ch)
                 elapsed (- (time-ms) t0)]
             [v elapsed])"#,
    );
    assert_eq!(result.kind(), ValueKind::Vector, "expected a vector");
    let items: Vec<Value> = result.iter().collect();
    assert_eq!(items[0].kind(), ValueKind::Nil);
    let elapsed = as_int(&items[1]);
    assert!((30..300).contains(&elapsed), "timeout 50 took {elapsed}ms");
}

// -------------------- alts!! --------------------

#[test]
fn alts_picks_the_ready_channel() {
    assert_eq!(
        ps(
            r#"(let [c1 (chan 1) c2 (chan 1)]
                 (>!! c2 :from-c2)
                 (let [[v ch] (alts!! [c1 c2])]
                   [v (= ch c2)]))"#
        ),
        "[:from-c2 true]"
    );
}

#[test]
fn alts_default_fires_when_nothing_ready() {
    assert_eq!(
        ps(r#"(let [c1 (chan) c2 (chan)] (alts!! [c1 c2] :default :nothing))"#),
        "[:nothing :default]"
    );
}

#[test]
fn alts_put_op_form() {
    assert_eq!(
        ps(
            r#"(let [ch (chan 1)]
                 (let [[ok c] (alts!! [[ch :val]])]
                   [ok (= c ch) (<!! ch)]))"#
        ),
        "[true true :val]"
    );
}

// -------------------- poll! / offer! --------------------

#[test]
fn poll_and_offer_non_blocking() {
    // Third `offer!` (`:c`) hits a full-but-open buffer -- "would block",
    // which real core.async's `offer!` reports as `nil`, distinct from the
    // `false` it reports for an already-closed channel (see
    // `builtins::async::register`'s `offer!` doc comment).
    assert_eq!(
        ps(
            r#"(let [ch (chan 1)]
                 [(poll! ch) (offer! ch :a) (poll! ch)
                  (offer! ch :b) (offer! ch :c) (poll! ch)])"#
        ),
        "[nil true :a true nil :b]"
    );
}

#[test]
fn offer_on_closed_channel_returns_false_not_nil() {
    // Distinguishes the closed case (`false`) from the would-block case
    // (`nil`, covered above) -- verified against babashka v1.13's
    // `clojure.core.async/offer!`.
    assert_eq!(ps(r#"(let [ch (chan 1)] (close! ch) (offer! ch :a))"#), "false");
}

// -------------------- go / go-loop --------------------

#[test]
fn go_returns_result_on_its_channel_then_closes() {
    assert_eq!(ps(r#"(let [ch (go (+ 1 2))] [(<!! ch) (<!! ch)])"#), "[3 nil]");
}

#[test]
fn go_loop_counting_0_through_9_via_channel() {
    assert_eq!(
        ps(
            r#"(let [ch (chan 10)
                     g (go-loop [i 0]
                         (if (< i 10)
                           (do (>! ch i) (recur (inc i)))
                           (close! ch)))]
                 (<!! g)
                 (loop [acc []]
                   (let [v (<!! ch)]
                     (if (nil? v) acc (recur (conj acc v))))))"#
        ),
        "[0 1 2 3 4 5 6 7 8 9]"
    );
}

#[test]
fn producer_consumer_go_loops_sum_exactly() {
    let result = eval_ok(
        r#"(let [ch (chan 10)
                 producer (go-loop [i 0]
                            (if (< i 100)
                              (do (>! ch i) (recur (inc i)))
                              (close! ch)))
                 consumer (go-loop [acc 0]
                            (let [v (<! ch)]
                              (if (nil? v) acc (recur (+ acc v)))))]
             (<!! producer)
             (<!! consumer))"#,
    );
    // sum of 0..99
    assert_eq!(as_int(&result), 4950);
}

#[test]
fn onto_chan_bang_puts_then_closes() {
    assert_eq!(
        ps(
            r#"(let [ch (chan 10)
                     done (onto-chan! ch [1 2 3])]
                 (<!! done)
                 (loop [acc []]
                   (let [v (<!! ch)]
                     (if (nil? v) acc (recur (conj acc v))))))"#
        ),
        "[1 2 3]"
    );
}

/// A 3-stage pipeline (`in -> stage1 (*2) -> mid -> stage2 (+1) -> out`),
/// each stage its own `go-loop`, wired end to end.
#[test]
fn pipeline_of_three_chained_go_loops() {
    assert_eq!(
        ps(
            r#"(let [in (chan 10)
                     mid (chan 10)
                     out (chan 10)
                     stage1 (go-loop []
                              (let [v (<! in)]
                                (if (nil? v)
                                  (close! mid)
                                  (do (>! mid (* v 2)) (recur)))))
                     stage2 (go-loop []
                              (let [v (<! mid)]
                                (if (nil? v)
                                  (close! out)
                                  (do (>! out (+ v 1)) (recur)))))
                     producer (go-loop [i 0]
                                (if (< i 5)
                                  (do (>! in i) (recur (inc i)))
                                  (close! in)))]
                 (<!! producer)
                 (<!! stage1)
                 (<!! stage2)
                 (loop [acc []]
                   (let [v (<!! out)]
                     (if (nil? v) acc (recur (conj acc v))))))"#
        ),
        "[1 3 5 7 9]"
    );
}

/// An error `throw`n inside a `go` body doesn't crash the process (it's
/// rendered to stderr per `go*`'s documented v0 behavior); the go channel
/// just closes, same observable shape as a `nil`-returning go body.
#[test]
fn go_body_error_closes_channel_without_panicking() {
    assert_eq!(ps(r#"(let [ch (go (throw :boom))] (<!! ch))"#), "nil");
}

// -------------------- put! / take! (async, callback) --------------------

#[test]
fn put_bang_and_take_bang_callbacks_fire() {
    assert_eq!(
        ps(
            r#"(let [ch (chan 1)
                     p1 (promise)
                     p2 (promise)]
                 (put! ch :hi (fn [sent] (deliver p1 sent)))
                 (deref p1 200 :put-never-fired)
                 (take! ch (fn [v] (deliver p2 v)))
                 [(deref p1 200 :put-never-fired) (deref p2 200 :take-never-fired)])"#
        ),
        "[true :hi]"
    );
}

// -------------------- errors --------------------

#[test]
fn putting_nil_is_an_error() {
    let msg = eval_err(r#"(let [ch (chan 1)] (>!! ch nil))"#);
    assert!(msg.contains("can't put nil on channel"), "unexpected message: {msg}");
}

// -------------------- chan? predicate --------------------

#[test]
fn chan_predicate() {
    assert_eq!(ps(r#"[(chan? (chan)) (chan? (atom 1)) (chan? 5)]"#), "[true false false]");
}
