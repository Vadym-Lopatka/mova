//! W-GEO kill-probe C: `geo-census` feature -- counts small-collection
//! allocation-shaped operations (`PVec`/`PMap` churn, promotion, access,
//! scan) as real workloads run, so Probe B's boxing-tax netting is
//! weighted by a MEASURED op mix instead of an invented one.
//!
//! This whole module -- and every call site that touches it in
//! `src/value.rs` -- is `#[cfg(feature = "geo-census")]`. The feature is
//! NOT in `default` (see `Cargo.toml`'s `[features]`), so an ordinary
//! `cargo build --release` compiles none of this in: no module, no call
//! sites, no counters, no branch to skip them. This is a stronger
//! guarantee than `builtins::map_probe`'s design (an always-compiled,
//! `OnceLock`-gated env-var check -- cheap, but still code and a load on
//! every touch site); `geo-census` is a diagnostic-only build
//! configuration, never meant to ship, so there is no reason to pay even
//! that.
//!
//! Counting is deliberately COARSE -- "cheap, measured not invented" per
//! the mission brief, not an exhaustive instrumentation of every `PVec`/
//! `PMap` method:
//!
//! * `churn` -- a persistent update that mutates/rebuilds the collection's
//!   storage: `PVec::push_back`'s `Small` arm, `PMap::insert`'s `Small`
//!   arm. (`PVec::update`/`PMap::update` thread through `set`/`insert`
//!   internally, so instrumenting the mutating primitive covers their
//!   non-mutating callers too, without double-counting.)
//! * `promote` -- the one-way `Small` -> `Big` transition, a submetric of
//!   `churn` (also counted in `churn`): counted once per genuine
//!   promotion, not per `to_big()` call (`to_big()` is also called on an
//!   already-`Big` receiver as a harmless clone in a few ops, which is NOT
//!   a promotion and must not inflate this count).
//! * `access` -- a single-element read: `PVec::get`, `PMap::get` (`first`/
//!   `front`/`last`/`back` all thread through one of these, so they are
//!   covered without a separate counter).
//! field4/W-LENS-1 ABSORPTION: the counters themselves are gone. Every
//! function below now forwards to `crate::lens`, the permanent regret-ledger
//! substrate, so these sites stop being a `static AtomicU64` `fetch_add` --
//! a shared-cacheline RMW on `PVec::get`/`PMap::get`, i.e. exactly the
//! measurement-poisoning shape the ledger's design principle 1 forbids --
//! and become one TLS load plus one uncontended relaxed load/store. Two
//! things deliberately did NOT change:
//!
//! * The CALL SITES stay `#[cfg(feature = "geo-census")]`. `access`/`scan`/
//!   `churn` sit on `PVec::get`/`PMap::get`/`PVec::iter`, the hottest reads
//!   in the runtime; keeping them feature-gated keeps a default build's
//!   behaviour byte-identical to what the overhead ruling was taken against.
//!   The one exception is `promote`, which W-LENS-1 counts UNCONDITIONALLY
//!   from `value.rs` (rare, one-way, and already in front of a full
//!   `to_big()` rebuild) -- so `pvec_promote`/`pmap_promote` below are now
//!   inert shims, kept only so this module's surface is unchanged for
//!   `tests/geo_census_probe.rs`.
//! * `Snapshot`'s shape and its `*_frac` helpers, so that probe still reads
//!   the same op-mix it was written against -- it just reads it out of the
//!   lens totals now.
//!
//! * `scan` -- one full traversal: `PVec::iter` is the only instrumented
//!   site (a `PMap`/`Set` traversal path was judged out of scope for the
//!   time this probe had -- see docs/W-GEO-PROBE-VERDICTS.md's caveat).
//!   Counted once per `iter()` CALL, not once per element, matching how
//!   `tests/geo_boxing_probe.rs` treats "one full scan" as one op unit.

use crate::lens::{self, Event};

#[inline]
pub fn pvec_churn() {
    lens::event(Event::PVecChurn);
}
/// Inert: `value.rs` counts promotion unconditionally through
/// `lens::Event::PVecPromote`, so counting here too would double it.
#[inline]
pub fn pvec_promote() {}
#[inline]
pub fn pvec_access() {
    lens::event(Event::PVecAccess);
}
#[inline]
pub fn pvec_scan() {
    lens::event(Event::PVecScan);
}
#[inline]
pub fn pmap_churn() {
    lens::event(Event::PMapChurn);
}
/// Inert; see [`pvec_promote`].
#[inline]
pub fn pmap_promote() {}
#[inline]
pub fn pmap_access() {
    lens::event(Event::PMapAccess);
}

/// A point-in-time read of every counter, summed across every thread's
/// counter page (see `crate::lens::totals`).
pub struct Snapshot {
    pub pvec_churn: u64,
    pub pvec_promote: u64,
    pub pvec_access: u64,
    pub pvec_scan: u64,
    pub pmap_churn: u64,
    pub pmap_promote: u64,
    pub pmap_access: u64,
}

pub fn snapshot() -> Snapshot {
    let t = lens::totals();
    let g = |e: Event| t.statics[e as usize];
    Snapshot {
        pvec_churn: g(Event::PVecChurn),
        pvec_promote: g(Event::PVecPromote),
        pvec_access: g(Event::PVecAccess),
        pvec_scan: g(Event::PVecScan),
        pmap_churn: g(Event::PMapChurn),
        pmap_promote: g(Event::PMapPromote),
        pmap_access: g(Event::PMapAccess),
    }
}

impl Snapshot {
    /// Total "collection-touching op" count across both types, the
    /// denominator the op-mix percentages are taken over.
    pub fn total(&self) -> u64 {
        self.pvec_churn + self.pvec_access + self.pvec_scan + self.pmap_churn + self.pmap_access
    }

    pub fn churn_frac(&self) -> f64 {
        (self.pvec_churn + self.pmap_churn) as f64 / self.total().max(1) as f64
    }
    pub fn access_frac(&self) -> f64 {
        (self.pvec_access + self.pmap_access) as f64 / self.total().max(1) as f64
    }
    pub fn scan_frac(&self) -> f64 {
        self.pvec_scan as f64 / self.total().max(1) as f64
    }
}
