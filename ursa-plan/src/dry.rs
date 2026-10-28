//! Dry runs: a plan built over empty inputs, for `schema()` and `explain()`
//! (#193).
//!
//! A dry run goes through the same code as `collect()`: the same guards, the
//! same lowering, the same DataFusion plans, so the schema it ends with is the
//! one a collect returns. What it skips is the work on rows: the host serves
//! empty tables and empty graph indexes, a scan reads its file's schema but no
//! rows, and a DataFusion plan is recorded rather than executed (its rows are an
//! empty batch with the plan's schema). Nothing a dry run builds reaches a cache
//! a real collect reads: the reshaped-topology cache is bypassed, and the
//! topology-build counter does not count its empty indexes.
//!
//! The mode is per thread, set for the extent of a [`DryRun`]. Everything that
//! checks it runs on the calling thread.

use std::cell::{Cell, RefCell};

use datafusion::logical_expr::LogicalPlan;

thread_local! {
    static DRY: Cell<bool> = const { Cell::new(false) };
    static PLANS: RefCell<Vec<LogicalPlan>> = const { RefCell::new(Vec::new()) };
}

/// Whether the current thread is in a dry run.
pub fn active() -> bool {
    DRY.with(Cell::get)
}

/// Record a DataFusion plan a dry run would have executed (for `explain()`).
pub fn record(plan: &LogicalPlan) {
    PLANS.with(|p| p.borrow_mut().push(plan.clone()));
}

/// A dry run on this thread, for as long as it lives.
pub struct DryRun {
    prev: bool,
    plans: Vec<LogicalPlan>,
}

impl DryRun {
    pub fn enter() -> DryRun {
        let prev = DRY.with(|d| d.replace(true));
        let plans = PLANS.with(|p| std::mem::take(&mut *p.borrow_mut()));
        DryRun { prev, plans }
    }

    /// The plans recorded so far, in the order they would have run.
    pub fn plans(&self) -> Vec<LogicalPlan> {
        PLANS.with(|p| p.borrow().clone())
    }
}

impl Drop for DryRun {
    fn drop(&mut self) {
        DRY.with(|d| d.set(self.prev));
        let outer = std::mem::take(&mut self.plans);
        PLANS.with(|p| *p.borrow_mut() = outer);
    }
}
