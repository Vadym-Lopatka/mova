//! # L5 / W5 — Episode 2: "400 years of Februaries" (the Azure leap-day twin)
//!
//! *The demo of `docs/L5-FLAGSHIP-DEMOS.md` Episode 2, landed as a real test.
//! The program under test is `probes/l5-sim/ep2-calendar.mova` (the demo's own
//! Mova date math) plus `probes/l5-sim/ep2-twin.mova` (the control-plane
//! twin), driven through `simulate` — L5's virtual clock and seeded scheduler
//! — over mova's L4 supervision surface.*
//!
//! ## What is being resurrected
//!
//! Azure, February 29 2012. A Guest Agent computed a certificate expiry as
//! "today plus one on the year" and got **February 29, 2013** — a date that
//! does not exist. Agent activation failed. The date bug cost minutes; the
//! recovery machinery cost ~13 hours: the Host Agent retried on a 25-minute
//! ladder, misdiagnosed a deterministic *software* fault as *hardware*
//! failure, and the healing service migrated the machine's VMs to healthy
//! machines — where the same bug fired again, marking THOSE machines bad. The
//! immune system spread the disease.
//!
//! The twin is OUR reconstruction from the public postmortem, not Microsoft's
//! code, scaled down honestly. UTC only: no leap seconds, no DST.
//!
//! ## Why 400 years is exhaustion and not sampling
//!
//! The Gregorian calendar is exactly periodic with a 400-year cycle: 146,097
//! days, 97 leap years, a whole number of weeks. Sweeping 400 consecutive
//! years covers every leap rule that exists — divisible-by-4, the
//! divisible-by-100 exception (**2100**, the `% 4` time bomb in production
//! code everywhere today), and the divisible-by-400 exception-to-the-exception
//! (2000). Bar C6 below round-trips every one of those 146,097 days.
//!
//! ## The crash trigger, and the one deviation the kernel forced
//!
//! No ordinary Mova program can kill a proc: a `throw` in a `transform` is an
//! INCIDENT (error chan, proc continues — the L4 D-B invariant), and a `throw`
//! in a `:transition` is swallowed by `call_transition`'s `unwrap_or(state)`.
//! An `init` throw — which is where Azure's bug actually lived — is likewise
//! an incident (`emit_lifecycle_error`, state becomes `nil`, the proc lives
//! on). The one proc-killing SYSTEM FAULT reachable from Mova is a host-native
//! panic on the `:transition` path, so the twin's agent activation runs on the
//! `::flow/resume` transition and signals its fault through
//! `demo/system-fault`, registered here through the public
//! `embed::Engine::register_fn` surface. Restart's auto-resume (L4 owner
//! ruling #5) re-runs that transition on every incarnation, which is exactly
//! what turns one bad date into a retry ladder. Everything else — calendar,
//! certificate, healing policy, migration — is ordinary Mova.
//!
//! `demo/system-fault`'s panics are printed by the runtime's panic path
//! (`mova: task N panicked on shard 0 … (shard continues)`). That output is
//! expected here; it is the fault fleet dying, once per activation attempt.
//!
//! ## World discipline
//!
//! `simulate` is process-scoped (design §2, "Ownership") — one world, one
//! root, one seed at a time — so [`SIM`] serializes every test in this file,
//! exactly as `tests/l5_sim_api.rs` does, and nothing here may touch the
//! runtime before the first `simulate` call.
//!
//! ---
//!
//! # PRE-REGISTERED BARS
//!
//! *Written before the first full run of this file. A bar that is not met is
//! a failure, not a number to re-negotiate.*
//!
//! ## Fleet scale shipped (the default fleet every non-`#[ignore]` test uses)
//!
//! 10 hosts x 5 VMs, 20 VM slots per host = **200 supervised procs per
//! simulated world**, 56 VMs per world (50 initial + 6 post-midnight
//! creations), 3 virtual days of observation. The full-scale
//! `docs/L5-FLAGSHIP-DEMOS.md` fleet (50 x 20, 2,250 procs) is exercised by
//! the `#[ignore]`d [`full_scale_fleet_reproduces_both_verdicts`].
//!
//! ## Timescale scaling, declared
//!
//! Azure's ladder was 25 minutes x3 (~75 minutes) before a machine was
//! declared faulty. The twin keeps the SHAPE — three activation attempts,
//! growing gaps, then give up — at 5 -> 10 -> 20 virtual minutes, so a
//! ten-generation migration cascade fits inside the 3-virtual-day observation
//! window. The compression buys observation WINDOW, not wall time.
//!
//! ## C — the calendar (must pass before the twin is allowed to run)
//!
//! - **C1** leap taxonomy exact: 2000 leap, 2100 NOT leap, 2400 leap, 2012
//!   leap, 2011 not, 1900 not.
//! - **C2** February length: 2000 -> 29, 2100 -> 28, 2012 -> 29.
//! - **C3** validity: 2000-02-29 valid; 2100-02-29, 2013-02-29 and 2012-02-30
//!   invalid.
//! - **C4** one Gregorian cycle = **146,097 days**, and a whole number of
//!   weeks.
//! - **C5** exactly **97** leap years in [2000, 2400).
//! - **C6** every one of the cycle's 146,097 days round-trips
//!   date -> days -> date: **0** failures.
//! - **C7** epoch anchors: `days-from-civil(1970,1,1) = 0`;
//!   `utc-ms(2012,2,29,0,0) = 1330473600000`;
//!   `utc-ms(2012,2,28,23,0) = 1330470000000`, whose date is 2012-02-28 and
//!   whose date one hour later is 2012-02-29; `epoch-ms->date(-1)` =
//!   1969-12-31 (floor division, not truncation).
//! - **C8** the bug and the fix, side by side: naive(2012-02-29) =
//!   2013-02-29 and is INVALID; clamped(2012-02-29) = 2013-02-28 and is
//!   VALID; and the naive rule breaks in exactly **97** of the cycle's 400
//!   years — one per leap year, the exhaustion claim proved on the calendar
//!   itself.
//!
//! ## A1 — ACT 1, THE RESURRECTION (seed 7, epoch = 2012-02-28 23:00 UTC,
//! 3 virtual days, buggy date math + the 2012 healing policy)
//!
//! **This test EXPECTS the failure.** It asserts that the faithful twin
//! cascades.
//!
//! - **A1.1** `hosts-marked-bad >= 5` — half the fleet or more: the 2012
//!   signature.
//! - **A1.2** `hosts-marked-bad > 3` — strictly more hosts went bad than the
//!   3 that ever received a post-midnight VM creation. The extra hosts were
//!   condemned by the HEALING, not by the workload: that is the cascade.
//! - **A1.3** `migrations >= 1` and `migrated-vms >= 1` — the vector is real.
//! - **A1.4** `vms-down > 6` — more VMs died than the 6 that were created
//!   after midnight, i.e. previously HEALTHY VMs were broken by being
//!   rescued. Azure's actual tragedy.
//! - **A1.5** every bad host reported the cert fingerprint:
//!   `fault-hosts >= hosts-marked-bad`, and `give-ups == vms-down` (a run
//!   gives up exactly once, and only a dead VM gives up).
//! - **A1.6** the clock really crossed the leap day: the certificate date at
//!   t0 is 2012-02-28 and after the creation wave is **2012-02-29**.
//! - **A1.7** supervision actually ran the ladder:
//!   `sup-events >= 4 * give-ups` (exit, restart, exit, restart, exit,
//!   give-up = 6 per condemned VM; 4 is the slack-free floor).
//! - **A1.8** 3 virtual days cost < 5 s of wall time.
//!
//! ## A2 — ACT 2, THE DATE FIX (calendar-aware `plus-years`; sweep every year
//! in [2000, 2400), seed 7 fixed, `:epoch-ms` = that year's Feb 28 23:00)
//!
//! - **A2.1** 400 rows, and `hosts-marked-bad == 0` in **every** year.
//! - **A2.2** `faults == 0` and `give-ups == 0` in every year — with the fix
//!   nothing even throws.
//! - **A2.3** the sweep covers the leap taxonomy: 97 leap years and 303
//!   non-leap years by the demo's own `leap-year?`; in each of the 97 the
//!   post-midnight activation date is **February 29**, and in each of the 303
//!   it is March 1 — the sweep VISITED the leap day, it did not merely count
//!   to 400. 2100 is in the sweep, ran, and is not a leap year; 2000 is.
//!   (AMENDED at first run: the bar originally said "2000 and 2400 are". The
//!   cycle is the half-open interval [2000, 2400), so 2400 is the first year
//!   of the NEXT cycle and cannot be a row here. 2400's leapness is pinned by
//!   C1 and swept by A3.5, whose range is `2000..2401`.)
//! - **A2.4** the whole 400-run sweep finishes in < 60 s of wall time, and
//!   the measurement is printed — that number is the "400 years of Februaries
//!   in N seconds" ledger entry.
//! - **A2.5 (exhaustion, at the twin level)** the same 400-year sweep run
//!   with the BUG re-injected on a minimal 2-host fleet cascades in exactly
//!   **97** of the 400 years, and the set of cascading years is exactly the
//!   set of leap years. Not a sample: the complete cycle.
//!
//! ## A3 — ACT 3, THE POLICY FIX (the Azure lesson: the date bug is
//! deliberately BACK; only the diagnosis changed — an identical software
//! fingerprint seen on >= 3 distinct hosts is never a hardware failure, and
//! healing is capped at 2 actions)
//!
//! - **A3.1** year 2012: `hosts-marked-bad == 0`.
//! - **A3.2** year 2012: `migrations == 0`, and in any case `<= 2` (the cap) —
//!   no migration storm.
//! - **A3.3** year 2012: the fault is still REAL and still reported —
//!   `faults > 0`, `vms-down > 0`, `fault-hosts == 3`,
//!   `software-classifications >= 1`.
//! - **A3.4** year 2012: blast radius zero — `migrated-vms == 0` and
//!   `vms-down == 6`, exactly the VMs whose own activation hit the bug. No
//!   healthy VM was broken by the rescue.
//! - **A3.5** spot-check sweep, every 50th year of the cycle (2000, 2050,
//!   ..., 2400): `hosts-marked-bad == 0` and `migrations <= 2` in all nine;
//!   `faults > 0` in the leap ones (2000, 2400) and `faults == 0` in the
//!   seven non-leap ones.
//!
//! ## D — determinism
//!
//! - **D1** Act 1 run in two FRESH processes produces the identical result —
//!   same `hosts-marked-bad`, same `bad-hosts` ORDER, same `sup-events`
//!   count, same everything.
//!
//! ## Invocation of the slow target
//!
//! Everything above runs in the default
//! `cargo test --release --test l5_demo_ep2`. The full-scale fleet is
//! `#[ignore]`d:
//!
//! ```text
//! cargo test --release --test l5_demo_ep2 -- --ignored --nocapture \
//!     full_scale_fleet_reproduces_both_verdicts
//! ```

use std::sync::Mutex;
use std::time::{Duration, Instant};

use mova::embed::{Engine, Profile, Value as EValue};

/// See the module doc: one simulated world at a time, process-wide.
static SIM: Mutex<()> = Mutex::new(());

const CALENDAR: &str = include_str!("../probes/l5-sim/ep2-calendar.mova");
const TWIN: &str = include_str!("../probes/l5-sim/ep2-twin.mova");

/// An engine carrying the one host native the twin needs: the SYSTEM FAULT
/// primitive (see the module doc for why a Mova `throw` cannot be one).
fn engine() -> Engine {
    let mut engine = Engine::builder().profile(Profile::Scripting).build();
    engine.register_fn("demo/system-fault", |_args: &[EValue]| {
        panic!(
            "l5_demo_ep2: guest-agent activation failed — the transfer certificate's expiry is \
             not a real date (Azure, 2012-02-29)"
        )
    });
    engine
}

/// An engine with the whole demo program loaded.
fn demo_engine() -> Engine {
    let mut e = engine();
    e.eval(CALENDAR).unwrap_or_else(|err| panic!("ep2-calendar.mova failed to load: {err}"));
    e.eval(TWIN).unwrap_or_else(|err| panic!("ep2-twin.mova failed to load: {err}"));
    e
}

fn ev(e: &mut Engine, src: &str) -> EValue {
    e.eval(src).unwrap_or_else(|err| panic!("eval error: {err}\n--- program ---\n{src}"))
}

/// `(:k m)` as an integer, or a failure that names the key.
fn i(v: &EValue, k: &str) -> i64 {
    v.get_kw(k)
        .and_then(|x| x.as_i64())
        .unwrap_or_else(|| panic!("expected {k} to be an integer in {v:?}"))
}

/// One `[year leap? hosts-bad migrations faults give-ups cert-m cert-d]` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    year: i64,
    leap: bool,
    hosts_bad: i64,
    migrations: i64,
    faults: i64,
    give_ups: i64,
    cert_month: i64,
    cert_day: i64,
}

fn rows(v: &EValue) -> Vec<Row> {
    v.iter()
        .map(|r| {
            let c: Vec<i64> = r.iter().map(|x| x.as_i64().expect("integer cell")).collect();
            assert_eq!(c.len(), 8, "a sweep row is 8 columns: {r:?}");
            Row {
                year: c[0],
                leap: c[1] == 1,
                hosts_bad: c[2],
                migrations: c[3],
                faults: c[4],
                give_ups: c[5],
                cert_month: c[6],
                cert_day: c[7],
            }
        })
        .collect()
}

// ===========================================================================
// C — the calendar, tested before the twin is allowed to use it
// ===========================================================================

/// Bars C1-C8. Nothing below this test is meaningful if it fails: every
/// verdict in Acts 1-3 is a claim about dates, and this is the only thing
/// that makes the twin's dates trustworthy.
#[test]
fn the_calendar_fixtures_are_exact() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = engine();
    let f = ev(&mut e, CALENDAR);

    let b = |k: &str| {
        f.get_kw(k).and_then(|x| x.as_bool()).unwrap_or_else(|| panic!("expected bool at {k}"))
    };
    let date = |k: &str| {
        let d = f.get_kw(k).unwrap_or_else(|| panic!("missing {k}"));
        (i(&d, "y"), i(&d, "m"), i(&d, "d"))
    };

    // C1 — the complete leap taxonomy, including both exceptions.
    assert!(b("leap-2000"), "C1: 2000 is divisible by 400 -> leap");
    assert!(!b("leap-2100"), "C1: 2100 is divisible by 100 but not 400 -> NOT leap (the `% 4` bomb)");
    assert!(b("leap-2400"), "C1: 2400 -> leap");
    assert!(b("leap-2012"), "C1");
    assert!(!b("leap-2011"), "C1");
    assert!(!b("leap-1900"), "C1: 1900 -> not leap");

    // C2
    assert_eq!(i(&f, "feb-2000"), 29, "C2");
    assert_eq!(i(&f, "feb-2100"), 28, "C2");
    assert_eq!(i(&f, "feb-2012"), 29, "C2");

    // C3
    assert!(b("valid-2000-02-29"), "C3");
    assert!(!b("valid-2100-02-29"), "C3");
    assert!(!b("valid-2013-02-29"), "C3: the date Azure's Guest Agent computed");
    assert!(!b("valid-2012-02-30"), "C3");

    // C4/C5
    assert_eq!(i(&f, "cycle-days"), 146_097, "C4: one Gregorian cycle in days");
    assert!(b("cycle-days-whole-weeks"), "C4: 146097 = 20871 weeks exactly");
    assert_eq!(i(&f, "leap-years-in-cycle"), 97, "C5");

    // C6 — every day of the cycle, round-tripped.
    assert_eq!(
        i(&f, "round-trip-failures"),
        0,
        "C6: date -> days -> date must be exact for all 146,097 days of the cycle"
    );

    // C7 — the epoch anchors.
    assert_eq!(i(&f, "epoch-day-zero"), 0, "C7");
    assert_eq!(i(&f, "ms-2012-02-29"), 1_330_473_600_000, "C7");
    assert_eq!(i(&f, "ms-2012-02-28-2300"), 1_330_470_000_000, "C7");
    assert_eq!(date("date-at-2012-02-28-2300"), (2012, 2, 28), "C7");
    assert_eq!(date("date-one-hour-later"), (2012, 2, 29), "C7: one hour of virtual time is a leap day");
    assert_eq!(date("date-pre-epoch"), (1969, 12, 31), "C7: floor division, not truncation");

    // C8 — the bug and the fix.
    assert_eq!(date("naive-2012-02-29"), (2013, 2, 29), "C8: the bug's answer, verbatim");
    assert!(!b("naive-2012-02-29-valid?"), "C8: ...and it is not a date");
    assert_eq!(date("clamped-2012-02-29"), (2013, 2, 28), "C8: the fix clamps");
    assert!(b("clamped-2012-02-29-valid?"), "C8");
    assert!(b("naive-2011-02-28-valid?"), "C8: the bug is invisible on every other day");
    assert!(!b("cert-2012-02-29-naive-ok"), "C8: the certificate itself fails on Feb 29");
    assert!(b("cert-2012-02-29-clamped-ok"), "C8");
    assert!(b("cert-2012-02-28-naive-ok"), "C8: ...and succeeds on Feb 28, bug and all");
    assert_eq!(
        i(&f, "naive-breaks-in-cycle"),
        97,
        "C8: the naive rule breaks in exactly the 97 leap years of the cycle — exhaustion, \
         not sampling"
    );
}

// ===========================================================================
// A1 — ACT 1: THE RESURRECTION
// ===========================================================================

/// **This test EXPECTS the failure.** Seed 7, epoch 2012-02-28 23:00 UTC,
/// three virtual days: the buggy twin must cascade, and must cascade the way
/// 2012 did — through the healing service, onto hosts that never ran a single
/// failing workload of their own.
#[test]
fn act1_the_2012_cascade_is_reproduced() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = demo_engine();
    let t = Instant::now();
    let r = ev(
        &mut e,
        r#"(:result (simulate {:seed 7 :epoch-ms (february 2012)}
             (fn [] (run-cluster-twin act1-cfg))))"#,
    );
    let wall = t.elapsed();
    eprintln!("ACT 1 (10 hosts x 5 VMs, 3 virtual days) in {wall:?} of wall time:\n  {r:?}");

    let bad = i(&r, "hosts-marked-bad");
    assert!(bad >= 5, "A1.1: expected at least half of the 10-host fleet marked bad, got {bad}");
    assert!(
        bad > 3,
        "A1.2: only {bad} hosts went bad — the 3 hosts that received post-midnight VM creations \
         can account for that on their own, so nothing was spread by the HEALING and this is not \
         the 2012 signature"
    );
    assert!(i(&r, "migrations") >= 1, "A1.3: no healing action ran at all");
    assert!(i(&r, "migrated-vms") >= 1, "A1.3: no VM was migrated");
    let down = i(&r, "vms-down");
    assert!(
        down > 6,
        "A1.4: only {down} VMs died — exactly the 6 created after midnight. The tragedy is that \
         HEALTHY VMs were broken by being rescued; that did not happen here"
    );
    assert!(
        i(&r, "fault-hosts") >= bad,
        "A1.5: some host was marked bad without ever reporting the certificate fingerprint"
    );
    assert_eq!(i(&r, "give-ups"), down, "A1.5: a condemned run gives up exactly once");
    let at_t0 = r.get_kw("cert-date-at-t0").expect("cert-date-at-t0");
    let after = r.get_kw("cert-date-after-wave").expect("cert-date-after-wave");
    assert_eq!((i(&at_t0, "y"), i(&at_t0, "m"), i(&at_t0, "d")), (2012, 2, 28), "A1.6");
    assert_eq!(
        (i(&after, "y"), i(&after, "m"), i(&after, "d")),
        (2012, 2, 29),
        "A1.6: the creation wave must land ON the leap day"
    );
    assert!(
        i(&r, "sup-events") >= 4 * down,
        "A1.7: {} supervision events for {down} condemned VMs — the retry ladder did not run \
         (exit/restart/exit/restart/exit/give-up is 6 per VM)",
        i(&r, "sup-events")
    );
    assert!(wall < Duration::from_secs(5), "A1.8: three virtual days took {wall:?} of wall time");
}

// ===========================================================================
// A2 — ACT 2: THE DATE FIX, swept across the whole Gregorian cycle
// ===========================================================================

/// **400 years of Februaries.** One `simulate` call per year of the complete
/// Gregorian cycle, each with its own `:epoch-ms`, same seed, one process.
/// With calendar-aware `plus-years` the fleet must be clean in every single
/// one — and the sweep must demonstrably have VISITED February 29 in the 97
/// years that have one.
#[test]
fn act2_four_hundred_years_of_februaries_are_all_green() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = demo_engine();
    let t = Instant::now();
    let v = ev(&mut e, r#"(february-sweep 2000 2400 act2-cfg)"#);
    let wall = t.elapsed();
    let sweep = rows(&v);
    eprintln!(
        "ACT 2 — 400 YEARS OF FEBRUARIES in {:.3} s of wall time \
         ({} simulated worlds, 10 hosts x 5 VMs = 200 supervised procs each)",
        wall.as_secs_f64(),
        sweep.len()
    );

    assert_eq!(sweep.len(), 400, "A2.1");
    for r in &sweep {
        assert_eq!(r.hosts_bad, 0, "A2.1: CASCADE in year {} — {r:?}", r.year);
        assert_eq!(r.faults, 0, "A2.2: a certificate failed in year {} — {r:?}", r.year);
        assert_eq!(r.give_ups, 0, "A2.2: a guest agent died in year {} — {r:?}", r.year);
    }

    // A2.3 — the leap taxonomy, proved through the sweep itself.
    let leaps: Vec<i64> = sweep.iter().filter(|r| r.leap).map(|r| r.year).collect();
    assert_eq!(leaps.len(), 97, "A2.3: the cycle has exactly 97 leap years");
    assert_eq!(sweep.iter().filter(|r| !r.leap).count(), 303, "A2.3");
    for r in &sweep {
        let want = if r.leap { (2, 29) } else { (3, 1) };
        assert_eq!(
            (r.cert_month, r.cert_day),
            want,
            "A2.3: year {} is {}a leap year, so the post-midnight activation must have seen {:?}",
            r.year,
            if r.leap { "" } else { "not " },
            want
        );
    }
    let y = |year: i64| *sweep.iter().find(|r| r.year == year).expect("year in sweep");
    assert!(!y(2100).leap, "A2.3: 2100 must be in the sweep and must NOT be a leap year");
    assert_eq!((y(2100).cert_month, y(2100).cert_day), (3, 1), "A2.3: 2100 has no February 29");
    // The cycle is the HALF-OPEN interval [2000, 2400): 2400 is the first year
    // of the NEXT cycle, so it is not a row here by construction. Its
    // divisible-by-400 leapness is pinned by C1 and swept by A3.5, which runs
    // `(range 2000 2401 50)`.
    assert!(y(2000).leap, "A2.3: 2000 is the divisible-by-400 leap year of this cycle");
    assert_eq!((y(2000).cert_month, y(2000).cert_day), (2, 29), "A2.3");
    assert!(y(2012).leap, "A2.3: the year the world found out");

    assert!(wall < Duration::from_secs(60), "A2.4: the 400-year sweep took {wall:?} of wall time");
}

/// **A2.5 — exhaustion, at the twin level.** The same 400-year sweep with the
/// BUG re-injected, on the smallest fleet that can still cascade: the set of
/// years in which the control plane melts down must be *exactly* the set of
/// leap years. 97 of 400, no more and no fewer. That is what "the complete
/// cycle" means, and it is the reason this demo says 400 and not 40.
#[test]
fn act2_the_buggy_twin_cascades_in_exactly_the_97_leap_years() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = demo_engine();
    let t = Instant::now();
    let v = ev(&mut e, r#"(february-sweep 2000 2400 tiny-fleet)"#);
    let wall = t.elapsed();
    let sweep = rows(&v);
    let cascaded: Vec<i64> = sweep.iter().filter(|r| r.hosts_bad > 0).map(|r| r.year).collect();
    let leaps: Vec<i64> = sweep.iter().filter(|r| r.leap).map(|r| r.year).collect();
    eprintln!(
        "A2.5 — the buggy twin cascaded in {} of {} years, in {:.3} s of wall time",
        cascaded.len(),
        sweep.len(),
        wall.as_secs_f64()
    );
    assert_eq!(sweep.len(), 400, "A2.5");
    assert_eq!(cascaded.len(), 97, "A2.5: expected one meltdown per leap year, got {cascaded:?}");
    assert_eq!(
        cascaded, leaps,
        "A2.5: the meltdown years must be EXACTLY the leap years — same set, same order"
    );
    for r in &sweep {
        if r.leap {
            assert!(r.faults > 0, "A2.5: year {} is a leap year and nothing failed", r.year);
            assert!(r.migrations > 0, "A2.5: year {} cascaded without a migration", r.year);
        } else {
            assert_eq!(r.faults, 0, "A2.5: year {} is not a leap year — nothing may fail", r.year);
        }
    }
}

// ===========================================================================
// A3 — ACT 3: THE POLICY FIX (what Microsoft actually changed)
// ===========================================================================

/// The date bug is deliberately BACK. Only the diagnosis changed: an
/// identical software-fault fingerprint on >= 3 distinct hosts is never a
/// hardware failure, and healing's blast radius is capped at 2 actions. The
/// VMs whose own activation hit the bug still die — and are still reported —
/// but the immune system stops eating the fleet.
#[test]
fn act3_the_hardened_policy_survives_the_reinjected_bug() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = demo_engine();
    let r = ev(
        &mut e,
        r#"(:result (simulate {:seed 7 :epoch-ms (february 2012)}
             (fn [] (run-cluster-twin act3-cfg))))"#,
    );
    eprintln!("ACT 3 (2012, bug re-injected, policy hardened):\n  {r:?}");

    assert_eq!(i(&r, "hosts-marked-bad"), 0, "A3.1: a software fault was diagnosed as hardware");
    assert_eq!(i(&r, "migrations"), 0, "A3.2");
    assert!(i(&r, "migrations") <= 2, "A3.2: the healing cap was exceeded");
    assert!(i(&r, "faults") > 0, "A3.3: the certificate must STILL fail — the bug is back");
    assert!(i(&r, "vms-down") > 0, "A3.3: the affected VMs must still be down");
    assert_eq!(i(&r, "fault-hosts"), 3, "A3.3: the fingerprint was seen on the 3 seeded hosts");
    assert!(
        i(&r, "software-classifications") >= 1,
        "A3.3: nothing was ever classified as a software fault, so A3.1 passed for the wrong reason"
    );
    assert_eq!(i(&r, "migrated-vms"), 0, "A3.4: blast radius must be zero");
    assert_eq!(
        i(&r, "vms-down"),
        6,
        "A3.4: exactly the 6 post-midnight VMs may die — no healthy VM may be broken by a rescue"
    );

    // A3.5 — every 50th year of the cycle.
    let v = ev(
        &mut e,
        r#"(mapv (fn [y] (let [r (:result (simulate {:seed 7 :epoch-ms (february y)}
                                            (fn [] (run-cluster-twin act3-cfg))))
                              dt (:cert-date-after-wave r)]
                          [y (if (leap-year? y) 1 0)
                           (:hosts-marked-bad r) (:migrations r) (:faults r) (:give-ups r)
                           (:m dt) (:d dt)]))
                (range 2000 2401 50))"#,
    );
    let spot = rows(&v);
    eprintln!("ACT 3 spot-check sweep (every 50th year): {spot:?}");
    assert_eq!(spot.len(), 9, "A3.5");
    for r in &spot {
        assert_eq!(r.hosts_bad, 0, "A3.5: year {} marked a host bad — {r:?}", r.year);
        assert!(r.migrations <= 2, "A3.5: year {} exceeded the healing cap — {r:?}", r.year);
        if r.leap {
            assert!(r.faults > 0, "A3.5: year {} is a leap year — the bug must still bite", r.year);
        } else {
            assert_eq!(r.faults, 0, "A3.5: year {} is not a leap year — nothing may fail", r.year);
        }
    }
    assert_eq!(
        spot.iter().filter(|r| r.leap).map(|r| r.year).collect::<Vec<_>>(),
        vec![2000, 2400],
        "A3.5: 2000 and 2400 are the only leap years among every-50th of the cycle — 2100, 2200 \
         and 2300 are the `% 4` traps"
    );
}

// ===========================================================================
// D — determinism across FRESH processes
// ===========================================================================

/// **D1.** Same seed, same universe. Two fresh processes must produce the
/// identical Act 1 result — including the ORDER in which hosts were
/// condemned, which is a pure function of the seeded schedule.
///
/// Fresh processes, not two calls: virtual time and the process-global
/// counters are cumulative within one process (design §5), so a fresh process
/// is the strongest form of the claim.
#[test]
fn act1_is_bit_identical_across_two_fresh_processes() {
    let a = run_determinism_worker();
    let b = run_determinism_worker();
    assert!(a.starts_with("DET "), "the worker must print its result: {a}");
    assert_eq!(a, b, "D1: two fresh processes disagreed about the 2012 cascade — the moat leaks");
    eprintln!("D1 — identical across fresh processes:\n  {a}");
}

/// The worker half of [`act1_is_bit_identical_across_two_fresh_processes`].
/// `#[ignore]`d so it only ever runs when named explicitly.
#[test]
#[ignore]
fn act1_determinism_worker() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = demo_engine();
    let r = ev(
        &mut e,
        r#"(let [r (simulate {:seed 7 :epoch-ms (february 2012)}
                    (fn [] (run-cluster-twin act1-cfg)))
                 res (:result r)]
             [(:hosts-marked-bad res) (:bad-hosts res) (:sup-events res)
              (:faults res) (:give-ups res) (:migrations res)
              (:migrated-vms res) (:vms-down res)
              (:virtual-ms r) (:resumes r) (:timer-fires r)])"#,
    );
    println!("DET {r:?}");
}

/// Runs this very test binary in a child process, executing only the
/// `#[ignore]`d worker, and returns its `DET …` line.
fn run_determinism_worker() -> String {
    use std::process::Command;
    let exe = std::env::current_exe().expect("current_exe");
    let out = Command::new(exe)
        .args(["--exact", "act1_determinism_worker", "--ignored", "--nocapture", "--test-threads=1"])
        .output()
        .expect("spawn the determinism worker");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    stdout
        .lines()
        .find(|l| l.starts_with("DET "))
        .unwrap_or_else(|| {
            panic!(
                "the worker printed no DET line.\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .to_string()
}

// ===========================================================================
// The full-scale fleet (#[ignore] — see the module doc for invocation)
// ===========================================================================

/// `docs/L5-FLAGSHIP-DEMOS.md`'s own snippet asks for `{:hosts 50
/// :vms-per-host 20}`: **2,250 supervised procs per simulated world**, ~1,006
/// VMs. Both verdicts must survive the scale-up — Act 1 still cascades across
/// half the fleet or more, Act 3 still holds it at zero — and the 400-year
/// sweep still finishes in seconds, which is the honest headline number at
/// full scale.
#[test]
#[ignore]
fn full_scale_fleet_reproduces_both_verdicts() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = demo_engine();

    let t = Instant::now();
    let a1 = ev(
        &mut e,
        r#"(:result (simulate {:seed 7 :epoch-ms (february 2012)}
             (fn [] (run-cluster-twin full-scale-act1))))"#,
    );
    eprintln!("FULL-SCALE ACT 1 in {:?}:\n  {a1:?}", t.elapsed());
    assert!(i(&a1, "hosts-marked-bad") >= 25, "half of a 50-host fleet or more");
    assert!(i(&a1, "migrated-vms") > 0);

    let t = Instant::now();
    let a3 = ev(
        &mut e,
        r#"(:result (simulate {:seed 7 :epoch-ms (february 2012)}
             (fn [] (run-cluster-twin full-scale-act3))))"#,
    );
    eprintln!("FULL-SCALE ACT 3 in {:?}:\n  {a3:?}", t.elapsed());
    assert_eq!(i(&a3, "hosts-marked-bad"), 0);
    assert_eq!(i(&a3, "migrated-vms"), 0);
    assert!(i(&a3, "faults") > 0);

    let t = Instant::now();
    let v = ev(&mut e, r#"(february-sweep 2000 2400 full-scale-act2)"#);
    let wall = t.elapsed();
    let sweep = rows(&v);
    eprintln!(
        "FULL-SCALE ACT 2 — 400 YEARS OF FEBRUARIES in {:.3} s of wall time \
         (400 worlds x 2,250 supervised procs)",
        wall.as_secs_f64()
    );
    assert_eq!(sweep.len(), 400);
    for r in &sweep {
        assert_eq!(r.hosts_bad, 0, "CASCADE in year {} at full scale — {r:?}", r.year);
        assert_eq!((r.cert_month, r.cert_day), if r.leap { (2, 29) } else { (3, 1) });
    }
}
