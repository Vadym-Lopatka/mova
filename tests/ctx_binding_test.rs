//! L1/W1 (docs/L1-LANDING-SPEC.md §W1): dynamic `binding` frames are keyed
//! on an execution-CONTEXT id (`src/ctx.rs`) instead of a `ThreadId`.
//!
//! Two claims, and they pull in opposite directions:
//!
//! 1. Nothing observable changed. With no tasks in the process a thread
//!    mints one ctx id and keeps it, so `binding`, conveyance into
//!    `future`, `push-thread-bindings`, and per-thread isolation behave
//!    exactly as they did when the key was `ThreadId`. The first three
//!    tests are Mova-level and would have passed before this change --
//!    that is the point of them.
//! 2. A single OS thread can now hold SEVERAL independent binding stacks
//!    and switch between them. That is what the ctx id was introduced for
//!    and it has no Mova-level expression yet (the scheduler that would
//!    drive it is W2), so the last test drives `set_ctx` +
//!    `take/install_binding_locals` by hand exactly the way a shard's
//!    switch will, and asserts the isolation the M:1 multiplexing depends
//!    on. Without it, two `go` tasks on one shard would share one stack per
//!    var -- silent corruption, which is why §3.4 calls this a landing
//!    precondition rather than an optimization.

use std::sync::{Arc, Barrier};

use mova::embed::Engine;
use mova::internal::ctx::{
    current_ctx, fresh_ctx, install_binding_locals, set_ctx, take_binding_locals, BindingLocals,
};
use mova::internal::env::{Env, VarCell};
use mova::internal::{Symbol, Value};

fn ps(src: &str) -> String {
    Engine::builder()
        .build()
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
        .to_string()
}

/// A fresh, never-interned-anywhere-else cell to hang frames off, so these
/// tests can't be perturbed by (or perturb) any other var in the process.
fn fresh_cell(name: &str) -> Arc<VarCell> {
    Env::new_root().intern(&Symbol::simple(name))
}

fn top(cell: &VarCell) -> Option<i64> {
    match cell.current_binding() {
        Some(Value::Int(n)) => Some(n),
        Some(other) => panic!("expected an int binding, got {other:?}"),
        None => None,
    }
}

// -------------------- claim 1: nothing observable changed --------------------

/// Conveyance (`env::snapshot_thread_bindings` -> `BindingConveyance`)
/// reads the SPAWNING context's live frames and replays them in the spawned
/// one. It needed no code change -- it never named a thread id, it called
/// `current_binding` -- but it is the load-bearing consumer of the keying,
/// so it gets the gate test.
#[test]
fn binding_conveys_into_a_future() {
    assert_eq!(ps("(def ^:dynamic *x* 0) (binding [*x* 42] (deref (future *x*)))"), "42");
}

/// The frame must be visible at depth and through the future's own call
/// stack, not just as a top-level read of the var.
#[test]
fn binding_conveys_through_a_called_fn() {
    assert_eq!(
        ps(r#"(def ^:dynamic *x* 0)
              (defn peek-x [] *x*)
              (binding [*x* 7] (deref (future (peek-x))))"#),
        "7"
    );
}

/// The low-level `push-thread-bindings`/`pop-thread-bindings` pair (below
/// the `binding` macro; `PUSH_FRAMES` is the half of `BindingLocals` that
/// exists for it) still round-trips: the frame is visible between the two
/// calls and the var is back to its root after.
#[test]
fn push_pop_thread_bindings_round_trips() {
    assert_eq!(
        ps(r#"(def ^:dynamic *x* 1)
              (let [before *x*
                    _ (push-thread-bindings {(var *x*) 5})
                    during *x*
                    _ (pop-thread-bindings)
                    after *x*]
                [before during after])"#),
        "[1 5 1]"
    );
}

/// Regression guard for the keying change itself: distinct OS threads must
/// still land in distinct map entries. Barriers make the overlap real --
/// each thread reads its own frame while the other's frame is definitely
/// also live -- so a keying bug that collapsed the two contexts into one
/// entry fails here deterministically instead of by luck of scheduling.
#[test]
fn two_os_threads_keep_separate_binding_stacks() {
    let cell = fresh_cell("ctx-test-two-threads");
    let gate = Arc::new(Barrier::new(2));

    let mut handles = Vec::new();
    for n in [1_i64, 2] {
        let cell = cell.clone();
        let gate = gate.clone();
        handles.push(std::thread::spawn(move || {
            cell.push_binding(Value::Int(n));
            gate.wait(); // both frames now live at once
            assert_eq!(top(&cell), Some(n), "thread bound {n} but sees another context's frame");
            gate.wait(); // hold both frames until both have read
            cell.pop_binding();
        }));
    }
    for h in handles {
        h.join().expect("binding thread panicked");
    }

    // This thread never bound the cell, and both frames are gone anyway.
    assert_eq!(top(&cell), None);
}

// -------------------- claim 2: multiplexing on ONE thread --------------------

/// The whole reason `ctx.rs` exists, simulated without the scheduler: one
/// OS thread alternating between two contexts, each with its own frame on
/// the SAME var. Under `ThreadId` keying every assertion below that names a
/// context's own value would see the other's.
///
/// The switch here is exactly what W2's shard loop will do -- `set_ctx` plus
/// a `BindingLocals` swap, in that order, both halves every time -- so this
/// doubles as the executable spec for that sequence.
#[test]
fn one_thread_alternating_contexts_keeps_stacks_isolated() {
    let cell = fresh_cell("ctx-test-multiplex");
    let (a, b) = (fresh_ctx(), fresh_ctx());
    let host = current_ctx(); // this thread's own id, restored at the end

    // Switch in A (the host context owns no frames, so its locals are the
    // empty ones `take` leaves behind).
    let host_locals = take_binding_locals();
    set_ctx(a);
    install_binding_locals(BindingLocals::default());
    cell.push_binding(Value::Int(10));
    assert_eq!(top(&cell), Some(10));

    // A -> B. B has never run: fresh empty locals, and no frame on the var.
    let a_locals = take_binding_locals();
    set_ctx(b);
    install_binding_locals(BindingLocals::default());
    assert_eq!(top(&cell), None, "B must not see A's frame");
    cell.push_binding(Value::Int(20));
    assert_eq!(top(&cell), Some(20));
    // Nesting inside B must stack on B's frame, not on A's.
    cell.push_binding(Value::Int(21));
    assert_eq!(top(&cell), Some(21));

    // B -> A. A's frame is untouched by everything B did.
    let b_locals = take_binding_locals();
    set_ctx(a);
    install_binding_locals(a_locals);
    assert_eq!(top(&cell), Some(10), "A's frame was clobbered by B's traffic");
    cell.pop_binding();
    assert_eq!(top(&cell), None);

    // A -> B, unwind B fully.
    let _a_locals = take_binding_locals();
    set_ctx(b);
    install_binding_locals(b_locals);
    assert_eq!(top(&cell), Some(21), "B's stack did not survive the round trip");
    cell.pop_binding();
    assert_eq!(top(&cell), Some(20));
    cell.pop_binding();
    assert_eq!(top(&cell), None);

    // Back to the host context, which must find its own (empty) state.
    let _b_locals = take_binding_locals();
    set_ctx(host);
    install_binding_locals(host_locals);
    assert_eq!(current_ctx(), host);
    assert_eq!(top(&cell), None);
}

/// Ctx ids are unique and a thread's own id is stable across reads -- the
/// property that makes "no tasks in the process" bit-identical to the old
/// `ThreadId` keying.
#[test]
fn ctx_ids_are_unique_and_stable_per_thread() {
    let mine = current_ctx();
    assert_ne!(mine, 0, "0 is the unassigned sentinel, never a live id");
    assert_eq!(mine, current_ctx());

    let other = std::thread::spawn(current_ctx).join().unwrap();
    assert_ne!(mine, other);

    assert_ne!(fresh_ctx(), fresh_ctx());
    assert_eq!(current_ctx(), mine, "minting task ids must not disturb the caller");
}

// -------------------- W5b: conveyance into go*/thread*/put!/take! --------------------
//
// `future*` (`builtins/conc.rs`) already conveys the spawning side's dynamic
// bindings into its child thread; `go*`/`thread*`/`put!`/`take!`
// (`builtins/async.rs`) did not (see `tests/task_stress_test.rs`'s
// `c2_binding_conveyance_from_go_into_future`, the bug report this landing
// fixes). These four are the direct spec for that fix: `thread*` conveys,
// `put!`/`take!`'s callbacks convey, and a `binding` re-established INSIDE a
// conveyed `go` body nests correctly on top of the conveyed frame instead of
// replacing or ignoring it.

/// `thread*` (the `thread` macro's native) is the always-OS-thread
/// placement `go*`/`put!`/`take!` fall back to under `MOVA_GO_THREADS=1` --
/// its conveyance is exercised directly here rather than only via the kill
/// switch, since `thread*` has no task placement to default to in the first
/// place.
#[test]
fn binding_conveys_into_a_thread() {
    assert_eq!(ps("(def ^:dynamic *x* 0) (<!! (binding [*x* 42] (thread *x*)))"), "42");
}

/// `put!`'s callback runs on the spawned task/thread same as a go body, so
/// it must see the same conveyance a bare `go`/`thread` body gets --
/// snapshotted at the `put!` call site, which is INSIDE the `binding` form
/// here (the binding form itself has already returned by the time the
/// callback actually runs, so this also proves the snapshot -- not a live
/// reference to the caller's frame -- is what the callback sees).
#[test]
fn binding_conveys_into_a_put_bang_callback() {
    assert_eq!(
        ps(r#"(def ^:dynamic *x* 0)
              (let [ch (chan 1)
                    p (promise)]
                (binding [*x* 42]
                  (put! ch :go (fn [_] (deliver p *x*))))
                (deref p 200 :put-callback-never-fired))"#),
        "42"
    );
}

/// Same claim as the `put!` test above, for `take!`.
#[test]
fn binding_conveys_into_a_take_bang_callback() {
    assert_eq!(
        ps(r#"(def ^:dynamic *x* 0)
              (let [ch (chan 1)
                    p (promise)]
                (>!! ch :hi)
                (binding [*x* 7]
                  (take! ch (fn [_] (deliver p *x*))))
                (deref p 200 :take-callback-never-fired))"#),
        "7"
    );
}

/// A `binding` established INSIDE a `go` body that was itself spawned under
/// an outer `binding` must nest on top of the conveyed frame: the inner
/// value wins while the inner `binding` form is active, and once it exits
/// the var reads back as the CONVEYED outer value (1), not the var's root
/// (0). Before this landing `*x*` inside the `go` body read 0 the whole
/// way through -- the outer `binding` never reached the task at all.
#[test]
fn nested_binding_inside_go_inner_wins_and_outer_restored_after() {
    assert_eq!(
        ps(r#"(def ^:dynamic *x* 0)
              (<!! (binding [*x* 1]
                     (go
                       (let [inner (binding [*x* 2] *x*)]
                         [inner *x*]))))"#),
        "[2 1]"
    );
}
