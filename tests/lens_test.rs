//! field4/W-LENS-1: the runtime regret ledger's gates.
//!
//! Five properties, each one a thing the ledger would be a liar without:
//!
//! 1. Counter pages register per THREAD and aggregate across all of them.
//! 2. Counters are MONOTONE -- gate 4's contract, and the thing a
//!    `:reset` must not break (it records a baseline; it never writes
//!    another thread's page).
//! 3. `:lens/window` is that baseline subtraction, and it windows.
//! 4. The watchdog is rate-limited: once per site per threshold crossing.
//! 5. The EDN report has `:lens/schema` and every key is namespaced, so a
//!    consumer written against v1.0.0 keeps parsing across minor bumps.
//!
//! The ledger is PROCESS-WIDE by design (regret is a property of the
//! runtime, not of one `Engine`), and `cargo test` runs a file's tests in
//! parallel threads of ONE process -- so a sibling test merely BUILDING an
//! engine bumps the same counters this one is asserting deltas on. Every
//! test here therefore takes [`serial`] first. That is not flakiness
//! papered over: it is the same property these tests exist to prove, seen
//! from the other side.

use std::sync::{Mutex, MutexGuard, OnceLock};

use mova::embed::{Engine, Profile};
use mova::internal::lens::{self, Event};

/// Serializes the tests that touch process-wide lens state.
fn serial() -> MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn total(ev: Event) -> u64 {
    lens::totals().statics[ev as usize]
}

/// Property 1: a page is per-thread, registers itself, and report time sums
/// every one of them -- including threads that have already exited.
#[test]
fn pages_register_per_thread_and_aggregate() {
    let _g = serial();
    let before = total(Event::PMapPromote);
    let pages_before = lens::totals().pages;
    let threads = 6u64;
    let per = 500u64;
    let mut handles = Vec::new();
    for _ in 0..threads {
        handles.push(std::thread::spawn(move || {
            for _ in 0..per {
                lens::event(Event::PMapPromote);
            }
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
    let t = lens::totals();
    assert_eq!(
        t.statics[Event::PMapPromote as usize] - before,
        threads * per,
        "every spawned thread's page must be summed, exited or not"
    );
    assert!(
        t.pages >= pages_before + threads as usize,
        "each thread's first event must register a NEW page ({} -> {})",
        pages_before,
        t.pages
    );
}

/// Property 1b: an attributed hit reaches BOTH the site cell and the static
/// total, and an unattributed one still reaches the total.
#[test]
fn site_hits_and_unattributed_hits_both_reach_the_total() {
    let _g = serial();
    let site = lens::alloc_site(
        lens::SiteKind::TierBail,
        "lens-test/attributed",
        Some("attributed"),
        "lens_test.rs:0",
    );
    assert_ne!(site, lens::NO_SITE);
    let before_total = total(Event::TierBailExec);
    let before_site = lens::totals().sites.get(site as usize).copied().unwrap_or(0);
    lens::event_at(Event::TierBailExec, site);
    lens::event_at(Event::TierBailExec, lens::NO_SITE);
    let t = lens::totals();
    assert_eq!(t.statics[Event::TierBailExec as usize] - before_total, 2);
    assert_eq!(t.sites[site as usize] - before_site, 1);
}

/// Property 2 + 3: `:reset` records a baseline and NEVER zeroes a page, so
/// `:lens/events` is monotone forever while `:lens/window` windows.
#[test]
fn reset_windows_without_ever_zeroing_a_counter() {
    let _g = serial();
    lens::event(Event::PVecPromote);
    let raw_before = total(Event::PVecPromote);

    // Reset: baseline := now.
    let r = lens::report(true, 0);
    let epoch_a = int_at(&r, "lens/epoch");

    // The raw counter must not have moved (a reset writes no page).
    assert_eq!(
        total(Event::PVecPromote),
        raw_before,
        "reset must never zero or rewrite a counter page"
    );

    lens::event(Event::PVecPromote);
    lens::event(Event::PVecPromote);
    let r2 = lens::report(false, 0);

    assert_eq!(
        sub_int(&r2, "lens/events", "lens.event/pvec-promote"),
        raw_before + 2,
        ":lens/events is the raw monotone total, unaffected by the reset"
    );
    assert_eq!(
        sub_int(&r2, "lens/window", "lens.event/pvec-promote"),
        2,
        ":lens/window is the baseline subtraction"
    );
    assert_eq!(
        int_at(&r2, "lens/epoch"),
        epoch_a + 1,
        "the epoch counts resets, so a consumer can tell windows apart"
    );
}

/// Property 4: a threshold fires ONCE per site per crossing, not once per
/// report -- otherwise a host polling every second would get the same
/// warning every second forever.
#[test]
fn watchdog_is_rate_limited_to_one_firing_per_site() {
    let _g = serial();
    let site = lens::alloc_site(
        lens::SiteKind::Macro,
        "lens-test/noisy",
        Some("noisy"),
        "lens_test.rs:0",
    );
    // Drive the site past the macro threshold in one go.
    let limit = 500_000u64;
    for _ in 0..limit {
        lens::event_at(Event::MacroExpand, site);
    }
    let first: Vec<String> = lens::warning_lines()
        .into_iter()
        .filter(|l| l.contains("noisy"))
        .collect();
    assert_eq!(
        first.len(),
        1,
        "the crossing must produce exactly one warning, got {first:?}"
    );
    assert!(
        first[0].contains("re-expanded"),
        "the warning must name what the site did: {}",
        first[0]
    );
    // Cross again, harder. Still silent: the site has already warned.
    for _ in 0..limit {
        lens::event_at(Event::MacroExpand, site);
    }
    let second: Vec<String> = lens::warning_lines()
        .into_iter()
        .filter(|l| l.contains("noisy"))
        .collect();
    assert!(
        second.is_empty(),
        "a site must warn once per crossing, not once per check: {second:?}"
    );
}

/// Property 5: the schema contract a consumer parses against.
#[test]
fn report_carries_a_version_and_only_namespaced_keys() {
    let _g = serial();
    let r = lens::report(false, 7);
    let v = lens::as_embed_value(r);

    assert_eq!(
        v.get_kw("lens/schema").and_then(|s| s.as_str().map(str::to_owned)),
        Some(lens::SCHEMA_VERSION.to_string()),
        "every report must be self-describing"
    );

    // Top level: every key namespaced under `lens`.
    for (k, _) in v.entries() {
        let name = k.as_keyword().expect("every report key is a keyword");
        assert!(
            name.starts_with("lens/"),
            "top-level key {name:?} is not in the `lens` namespace"
        );
    }
    // Sub-maps: `lens.event/...`, `lens.gauge/...`.
    for (sub, prefix) in [("lens/events", "lens.event/"), ("lens/gauges", "lens.gauge/")] {
        let m = v.get_kw(sub).unwrap_or_else(|| panic!("{sub} missing"));
        let mut n = 0;
        for (k, _) in m.entries() {
            let name = k.as_keyword().expect("keyword key");
            assert!(name.starts_with(prefix), "{sub} key {name:?} lacks {prefix}");
            n += 1;
        }
        assert!(n > 0, "{sub} must not be empty");
    }
    // `:lens/window` mirrors `:lens/events` key-for-key, so a consumer can
    // read either with the same key set.
    let events = v.get_kw("lens/events").expect("events");
    let window = v.get_kw("lens/window").expect("window");
    assert_eq!(events.len(), window.len());
    for (k, _) in events.entries() {
        assert!(window.get(&k).is_some(), "window is missing {k:?}");
    }
    // The gauge the CALLER supplies is the caller's, verbatim.
    assert_eq!(
        v.get_kw("lens/gauges")
            .and_then(|g| g.get_kw("lens.gauge/globals-retired"))
            .and_then(|n| n.as_i64()),
        Some(7)
    );
}

/// The embed surface: `Engine::lens_report` returns the same map, and a
/// snapshot clone does NOT inherit the watchdog hook (the documented
/// contract -- the host re-registers per clone).
#[test]
fn engine_lens_report_and_hook_are_not_inherited_by_a_snapshot() {
    let _g = serial();
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let v = engine.lens_report();
    assert_eq!(
        v.get_kw("lens/schema").and_then(|s| s.as_str().map(str::to_owned)),
        Some(lens::SCHEMA_VERSION.to_string())
    );
    assert!(
        v.get_kw("lens/gauges")
            .and_then(|g| g.get_kw("lens.gauge/engines-created"))
            .and_then(|n| n.as_i64())
            .unwrap_or(0)
            >= 1,
        "building an engine must show up in the engine-count gauge"
    );

    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let f2 = fired.clone();
    engine.on_lens_warning(move |_w| {
        f2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    // The clone starts with no callback; nothing this crate does can make
    // the parent's closure fire through it.
    let mut clone = engine.snapshot();
    let _ = clone.lens_report();
    assert_eq!(
        fired.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a snapshot clone must not inherit the parent's lens hook"
    );
}

/// `(runtime-report)` from script is the same data, reachable without any
/// Rust at all -- and `(runtime-report :reset)` is accepted.
#[test]
fn runtime_report_is_callable_from_script() {
    let _g = serial();
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let v = engine
        .eval("(:lens/schema (runtime-report))")
        .expect("(runtime-report) must be a registered builtin");
    assert_eq!(v.as_str(), Some(lens::SCHEMA_VERSION));
    let n = engine
        .eval("(count (runtime-report :reset))")
        .expect(":reset must be accepted");
    assert!(n.as_i64().unwrap_or(0) >= 8, "the report has all its sections");
}

// --- helpers ---------------------------------------------------------------

fn int_at(v: &mova::internal::Value, key: &str) -> i64 {
    let w = lens::as_embed_value(v.clone());
    w.get_kw(key)
        .and_then(|x| x.as_i64())
        .unwrap_or_else(|| panic!("{key} missing or not an int"))
}

fn sub_int(v: &mova::internal::Value, outer: &str, inner: &str) -> u64 {
    let w = lens::as_embed_value(v.clone());
    w.get_kw(outer)
        .and_then(|m| m.get_kw(inner))
        .and_then(|x| x.as_i64())
        .unwrap_or_else(|| panic!("{outer}/{inner} missing")) as u64
}
