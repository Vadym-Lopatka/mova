# Embedding `mova`/`mova`: the example suite

Nine runnable examples (`examples/embed_*.rs`), ordered as a learning path
from "load a script and read a value" to "many interpreters, one process."
Each is self-contained, terminates on its own, and is worth reading before
you read the facade's own doc comments in `src/embed/`. Run any of them
with `cargo run --release --example embed_<name>`; the three long-running
ones (`embed_rules_hotreload`, `embed_live_repl`, `embed_plugin_manager`)
need a trailing `-- --demo` to run their scripted self-demo instead of
blocking for a human. A tenth, `flow_plugins`, is folder-style rather than
a single `.rs` file — see §10.

## 1. `embed_config` — the five-minute hello world

**Shows:** [`Profile::Pure`], [`Engine::eval_named`], typed extraction,
error handling for a malformed script.

An app loads a user-editable `.mova` config file, evaluates it under
`Profile::Pure` (no filesystem/network access from the SCRIPT itself —
only the host reads the file), and pulls typed Rust values back out of the
resulting map. A hand-edited, syntactically broken config fails with a
rendered diagnostic instead of a panic. **Take-away:** a keyword `Value`
is callable as a fn against a map through `Engine::call` — `(:app-name
config)`'s Rust-side equivalent — which is the cleanest way to pull fields
out of script-produced data without walking `Value::entries()` by hand.

## 2. `embed_plugin` — the host exposes capabilities, the script orchestrates

**Shows:** [`Engine::register_fn`], [`Engine::register_fn_with_arity`],
error propagation in both directions.

The host registers native Rust functions (an inventory lookup, a
stock-taking operation that can fail); a small script decides when and in
what order to call them, wrapping the fallible one in `try`/`catch`.
**Take-away:** a `register_fn` closure's `Err` becomes a normal
script-catchable exception, and an UNCAUGHT script error propagates
straight back out through `Engine::eval`'s own `Result` — the native/script
error channel is the same one, in both directions.

## 3. `embed_rules_hotreload` — reload without corrupting what's live

**Shows:** mtime polling (no `notify` dependency), [`Engine::snapshot`] as
a load-time isolation boundary, [`Error::is_incomplete_input`].

A background loop watches a rules file's mtime and, on change, loads the
new source into a FRESH snapshot of a template engine — never the
currently-live one — before swapping it in. A broken edit never corrupts
the rules already serving traffic; the host is told the reload failed and
keeps running the old rules. **Take-away:** `snapshot()` is not just a
concurrency primitive — "try loading into a scratch world, only commit on
success" is a load-time pattern worth reaching for even single-threaded.

## 4. `embed_pipeline` — the script defines topology, the host owns lifecycle

**Shows:** `core.async.flow` driven from a script, [`Engine::shutdown`],
[`ShutdownReport`].

A script builds a small flow (`:gen → :xform → :sink`); the host injects a
batch of events, drains transformed results off a channel, and calls
`Engine::shutdown()` to stop every flow the script created. **Take-away:**
batch `flow/inject` calls (one call, many messages) rather than one call
per message — an engine's own per-proc threads interleave, so relative
order across SEPARATE inject calls is not guaranteed the way order WITHIN
one batch is.

## 5. `embed_untrusted` — running script text you didn't write

**Shows:** [`Profile::Untrusted`], fuel, [`Error::is_fuel_exhausted`],
[`Engine::set_fuel`].

A hostile infinite loop gets cut off by its fuel budget instead of hanging
the host thread — and `try`/`catch` cannot swallow that exhaustion, so the
host always regains control. A legitimately expensive (but finite) script
fails the same way and succeeds once the host grants a bigger retry
budget. **Fuel is not a sandbox**: fuel bounds interpreter steps, not
native-call time or memory — pair it with a process watchdog for real
untrusted hosting. (This example also surfaces a sharp edge: a native like
`reduce`/`range` that loops internally in Rust ticks NO fuel at all — only
script-level `loop`/`recur`/fn-call does.)

## 6. `embed_live_repl` — a REPL into a running app's live state

**Shows:** [`host::ShapeBuilder`]/[`host::wrap_struct`] (zero-copy state
view), a Unix-socket server thread, per-connection [`Engine::snapshot`],
`*1`/`*2`/`*3` via [`Engine::def`].

The flagship demo: a toy app with live, mutating state (an uptime counter
ticking on a background thread) exposes a REPL over a Unix socket. Every
connection gets its own snapshot of a template engine with the SAME
process-wide state `def`'d in as `app` via `wrap_struct` — `(:queue-depth
app)` reads the live value with no serialization step. **Take-away:** this
is the shape a `#[derive(MovaStruct)]` (not yet implemented — see
`DESIGN-hoststruct-derive.md` §5.4) will shrink from ~10 lines of
`ShapeBuilder` registration to one derive attribute; the runtime contract
underneath doesn't change.

## 7. `embed_snapshot_pool` — one engine per worker

**Shows:** [`Engine::snapshot`] as the concurrency primitive, `Engine:
Send`.

A template engine is built once (paying the bootstrap cost — registering
every builtin, loading `core.mova` — a single time); N worker threads each
get an independent snapshot to mutate freely, no `Mutex`/`RwLock`
serializing them onto one interpreter. The example prints the timing
asymmetry between the one-time template build and the per-worker snapshot
cost directly, so the payoff of "build once, fork cheaply" is a number,
not a claim.

## 8. `embed_plugin_manager` — a program made of plugins, CRUD'd live

**Shows:** the capstone composition of 3 + 7: per-plugin
[`Engine::snapshot`] isolation, validate-in-a-candidate-before-swap,
directory reconciliation as the whole plugin lifecycle.

A running host ticks once a second; its behavior is entirely a directory
of `.mova` files, each defining one `(defn process [n] ...)`. Dropping a
file in ADDS a plugin next tick, saving an edit UPDATES it atomically (a
broken edit is rejected with a rendered diagnostic and the old version
keeps serving), and deleting the file REMOVES it — teardown is just
dropping that plugin's `Engine`. Interactive mode watches a real
directory you edit by hand; `-- --demo` scripts the full
create/update/break/delete lifecycle and terminates. **Take-away:** one
template engine + one snapshot per plugin is the entire architecture of
a live-modifiable Rust program — no dynamic linking, no restart, no
plugin ever able to corrupt a neighbor.

## 9. `embed_capture` — capturing `*out*`/`*err*` instead of the real streams

**Shows:** [`Engine::eval_capture`], the `*err*` real-stderr fallback
guarantee.

A well-behaved script's `println` output and a reflective script's
interpreter WARNING (`Reflection warning, ...`) both come back as plain
Rust `String`s instead of touching the process's real stdout/stderr — a
log pane, a structured audit record, or a test assertion can inspect
either without racing the host's own console output. The example also
runs the SAME reflective script through a plain `Engine::eval` call for
comparison, so you can watch the warning land on the terminal's real
stderr instead. **Take-away:** `eval_capture` is not just for stdout —
`*err*` is where every interpreter warning (reflection, boxed-math,
def-shadowing, ...) goes, and it is captured the same way `*out*` is,
via mova's own `with-out-str`/`with-err-str` mechanism pushed from the
Rust side.

## 10. `flow_plugins` — separation of concerns, plugin CRUD through the flow itself

**Shows:** a folder-style standalone example (`examples/flow_plugins/`,
cargo auto-discovers `examples/<name>/main.rs` the same way it discovers
`examples/<name>.rs`), `core.async.flow` procs split one-per-file with NO
shared knowledge of the wiring, a plugin registry that lives in a proc's
own state and is CRUD'd entirely via flow messages on a dedicated `:ctl`
input port.

Composes 4 and 8 into a stronger claim than either makes alone: not just
"topology lives in script, host owns lifecycle" (4) and not just "a
program made of plugins" (8), but a plugin *registry* that is itself a
flow proc, whose CRUD travels through the flow's own message-passing
instead of a side channel the host reaches into proc state with. Four
files — `topology.mova` (communication ONLY, zero business logic),
`procs/ingest.mova`, `procs/plugin_stage.mova`, `procs/sink.mova` (logic
ONLY, none aware of the wiring) — plus a `main.rs` that does lifecycle
ONLY: load the four scripts, start the flow, watch a plugins directory,
inject data + control messages, drain output, shut down. Run the scripted
self-demo with `cargo run --release --example flow_plugins -- --demo`, or
point it at a real directory (`cargo run --release --example flow_plugins
my-plugins`) and edit `.mova` plugin files while it runs — no restart.
**Take-away:** the `flow/inject` ordering caveat from §4 has a concrete
fix here — a control message and a data message injected in two separate
calls can race, so the host drains the plugin-stage's ack off `out-ch`
before injecting anything else, turning "register landed" into something
externally observable in order. See `examples/flow_plugins/README.md` for
the full architecture explainer.

---

This ordering doubles as the skeleton for the README's "Embedding"
chapter: 1–2 are the on-ramp (eval + native functions), 3–4 show script
text as live, host-managed state (hot-reload, flow topology), 5–6 are the
two "run untrusted/host-driven" flagships (fuel, live introspection), 7
closes on the concurrency story that differentiates this facade from a
plain interpreter wrapper, 8 composes the reload and snapshot pieces into
the "program made of plugins" end state, 9 shows the `*out*`/`*err*`
capture surface a host reaches for once it wants a script's console
output and interpreter warnings under its own control rather than on the
real streams, and 10 composes 4 and 8 again into a plugin registry that
is itself a flow proc, CRUD'd through the flow's own messages rather than
a side channel.
