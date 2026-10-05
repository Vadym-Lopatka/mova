# `flow_plugins` — separation of concerns on `core.async.flow`

A standalone, folder-style example (`examples/flow_plugins/main.rs` — cargo
auto-discovers `examples/<name>/main.rs` the same way it discovers
`examples/<name>.rs`, so this is example `flow_plugins` like any other).
It shows two things at once:

1. How to keep **communication** (which procs exist, how they're wired)
   and **logic** (what each proc actually computes) in separate files that
   don't know about each other.
2. How to build a **plugin system whose CRUD travels through the flow
   itself** — register/deregister messages are ordinary flow messages on a
   dedicated control port, not a side channel the host reaches into a
   proc's state with from outside.

## Run it

```text
cargo run --release --example flow_plugins -- --demo
```

Scripted, self-contained, terminates on its own — walks the full
create/update/reject/delete lifecycle against a temp directory and prints
every step.

```text
cargo run --release --example flow_plugins my-plugins
```

Interactive: watches `my-plugins/` (seeding a starter `stats.mova` if it's
empty), ticks the pipeline once a second, Ctrl-C quits. From another
terminal:

```text
echo '(fn [ev] (assoc ev :shout (str "value " (:value ev) "!")))' > my-plugins/shout.mova   # create
echo '(fn [ev] (assoc ev :shout (str (:value ev) "!!!")))'        > my-plugins/shout.mova   # update
rm my-plugins/shout.mova                                                                     # delete
```

Every edit changes the running program's behavior on the *next* tick. The
process never restarts.

## The concerns diagram

```
main.rs (Rust)              topology.mova                procs/*.mova
──────────────              ─────────────                ───────────
LIFECYCLE ONLY:              COMMUNICATION ONLY:          LOGIC ONLY:
  load the 4 .mova files        which procs exist            ingest.mova:
  flow/start, flow/resume      which :out feeds              stamp {:seq n :value v}
  watch the plugins dir          which :in                 plugin_stage.mova:
  inject data + control        no business logic              plugin registry + applier
  drain out-ch                 at all                       sink.mova:
  engine.shutdown()                                            def out-ch, forward to it
     |                              |                              |
     | knows: 4 filenames,          | knows: proc names,           | knows: nothing about
     |   port names, message        |   port names                |   wiring, the host,
     |   shapes                     |   (:ingest/:plugin-stage/    |   or each other
     |                              |    :sink), :in/:out/:ctl     |
     v                              v                              v
  NEVER touches a proc's        NEVER computes anything        NEVER references
  transform logic                                              topology or the host
```

Each box only knows what's on its own arrow. `main.rs` never contains a
line of pipeline logic — it doesn't know what `:squared` or `:via` mean.
`topology.mova` never contains a `(fn ...)` that does real work — it just
names three step vars and says which output feeds which input.
`procs/*.mova` never says `:ingest`, `:plugin-stage`, or `:sink` — those
names live only in `topology.mova`. Delete a proc's whole implementation
and rewrite it from scratch and topology.mova is unaffected, as long as the
step var name (`ingest-step`, `plugin-stage-step`, `sink-step`) stays the
same shape (a `flow/map->step` result with matching `:ins`/`:outs`).

## The pipeline

```
[host injects data]  ->  :ingest  ->  :plugin-stage  ->  :sink  ->  out-ch  [host drains]
                                          ^
                                          |
                             [host injects control on :ctl]
```

- **`:ingest`** (`procs/ingest.mova`) stamps whatever raw value the host
  injects into `{:seq n :value v}`, where `n` is a counter kept in this
  proc's own state (`:init` returns `{:seq 0}`).
- **`:plugin-stage`** (`procs/plugin_stage.mova`) is the interesting proc —
  see below.
- **`:sink`** (`procs/sink.mova`) does one thing: `(def out-ch (chan 64))`
  at the top of the file, and a `:transform` that puts every message it
  receives onto `out-ch`. That `def` is this proc's entire *contract* with
  the host: whatever the host does to read results back out, it reads
  `out-ch` by that name. `topology.mova` never has to know a channel is
  involved.

`topology.mova`'s `:conns` only wires `[:ingest :out]` to `[:plugin-stage
:in]` and `[:plugin-stage :out]` to `[:sink :in]`. `:ingest`'s `:in` and
`:plugin-stage`'s `:ctl` are left **unconnected** — an unconnected input
port is exactly what makes it *injectable*: the host reaches data in at
`[:ingest :in]` and control messages in at `[:plugin-stage :ctl]`, both via
`flow/inject`.

## `:plugin-stage` — plugins as flow messages, not a side channel

`:plugin-stage` declares **two** input ports:

```clojure
{:ins {:in {} :ctl {}} :outs {:out {}}}
```

Its `:transform` is called as `(fn [state cid msg] ...)`, where `cid` is
the input port *keyword* the message arrived on — `:in` or `:ctl`. That
one `cid` check is the entire dispatch:

- **`cid = :ctl`** — a control message, either
  `{:op :register :name :stats :fn <a plugin fn Value>}` or
  `{:op :deregister :name :stats}`. The proc updates its registry — a
  **vector** of `[name fn]` pairs kept in proc state (`:init` returns
  `{:plugins []}`), not a map, so registration order is preserved.
  Re-registering a name removes the old entry and appends the new one, so
  an *updated* plugin moves to the *end* of the application order — the
  demo's step 4 (`stats.mova` gains `:cubed`) shows this: after the
  re-register, `:active` becomes `[:shout :stats]`, not `[:stats :shout]`.
  The proc then **outputs an ack** on `:out`:
  `{:kind :plugin-event :op <op> :name <name> :active [<registered names>]}`.
- **`cid = :in`** — a data event. The proc threads it through every
  registered plugin fn, in registry order, each call wrapped in its own
  `try`/`catch` — see below — then stamps `:via` (the vector of plugin
  names that ran) and forwards to `:out`.

### Why the ack matters — the `flow/inject` ordering paper-cut

`flow/inject` returns as soon as the message is *queued*, not once every
downstream proc has processed it. Each proc runs on its own thread, so two
**separate** `flow/inject` calls into **different ports** race against
each other — there is no guarantee a control message injected before a
data message is *applied* before that data message reaches
`:plugin-stage`. (Within a *single* `flow/inject` call, the batch's
relative order IS preserved — see `embed_pipeline`'s take-away — but that
doesn't help across two calls into two different ports.)

The fix this example uses: **the host drains the ack from `out-ch` before
injecting anything else.** `System::inject_ctl` in `main.rs` does exactly
one `flow/inject` followed by exactly one `(<!! out-ch)` read, and nothing
else runs on that engine in between. Since a register/deregister ack is
the *only* thing that can reach `out-ch` before the host's next `pump`
call, draining it first is what makes "register landed" externally
observable before the next data event is sent. Skip this and you get a
flaky demo where a freshly-registered plugin sometimes does, sometimes
doesn't, show up in the very next pumped event.

### Why plugin errors don't kill the proc

Each plugin call in `apply-plugins` (`procs/plugin_stage.mova`) is wrapped
individually:

```clojure
(try
  (f acc)
  (catch e
    (assoc acc :plugin-errors (conj (get acc :plugin-errors []) name))))
```

If a plugin throws, the event that survives into the *next* plugin's call
is `acc` — the version from **before** the failing call, untouched, with
the failing plugin's name appended to `:plugin-errors`. One broken plugin
never corrupts the event for the plugins after it, never kills the
`:plugin-stage` proc, and never stops `:sink` (or anything downstream)
from getting a result. In the demo, `broken.mova` never even gets this
far — it's rejected at the smoke test (next section) before it's ever
registered.

## The plugin file convention

A plugin file's **last form** must evaluate to a one-argument fn over the
event map, returning a (possibly enriched) event map:

```clojure
(fn [ev] (assoc ev :squared (* (:value ev) (:value ev))))
```

The host (`System::try_register` in `main.rs`):

1. Reads the file.
2. `engine.eval_named(stem, src)` → the fn `Value`.
3. **Smoke-tests** it immediately: `engine.call(&f, &[sample_event()])`
   against `{:seq 0 :value 1}` — the same shape `procs/ingest.mova`
   actually produces. Any error (syntax error in step 2, or a runtime
   error in step 3) **rejects** the plugin: the rendered diagnostic is
   printed and whatever version was previously registered (if any) stays
   live. Nothing about the running flow changes.
4. On success, builds `{:op :register :name <kw> :fn <the fn Value>}`
   with `Value::map` — the actual fn Value travels through `flow/inject`
   as ordinary flow data — and injects it in **one** call, then drains
   **exactly one** ack (see above).

## Host reconciliation (directory ↔ registry)

`PluginWatcher::sync` in `main.rs` diffs the plugins directory against an
`mtime` map it kept from the last sync — the same
`std::fs::metadata(..).modified()` polling `embed_plugin_manager.rs` uses
(no `notify` dependency):

- **new file** → register.
- **mtime changed** → re-register (replace; moves to the end of order).
- **file gone** → deregister + drain ack.
- **rejected reload** (smoke test failed) → the mtime is still recorded,
  so a broken file is not re-loaded and re-printed every single tick —
  only on its *next* edit.

## Why the split holds up

Try deleting `procs/plugin_stage.mova` and writing a completely different
plugin-stage implementation from scratch (different registry
representation, different ack shape) that still exposes a
`plugin-stage-step` var with `:ins {:in {} :ctl {}}` and `:outs {:out
{}}`. `topology.mova` doesn't change. Try adding a fourth proc between
`:plugin-stage` and `:sink` — only `topology.mova` changes; `sink.mova`
still doesn't know what feeds it. Try swapping `main.rs`'s polling loop
for something event-driven — no `.mova` file changes at all. That's the
whole point of drawing the line where this example draws it.
