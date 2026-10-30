//! Deep plans on any thread stack (#216).
//!
//! A plan nests one level per step, so a walk that recurses into a step's
//! input recurses as deep as the chain is long: 20,000 filters is 20,000
//! frames. Ursa's own walks over a chain (clone, drop, lowering, the executor,
//! the reshaped-topology fingerprint) are iterative. Serializing a plan and
//! `Debug` stay recursive and run through [`grow`], which moves onto a fresh
//! stack segment on the heap when the current one runs low; parsing one runs on
//! a stack sized to its JSON ([`for_json`]). DataFusion's walks over
//! the plans the steps lower to are recursive too, so a long plan's collect,
//! schema and explain run on a stack sized to it ([`for_steps`]).

use crate::plan::{PlanError, PlanResult};

/// The stack a guarded walk needs left before its next level: more than any
/// one level uses between two guards.
const RED_ZONE: usize = 128 * 1024;

/// The size of each segment a guarded walk moves onto.
const SEGMENT: usize = 4 * 1024 * 1024;

/// Run `f`, on a new stack segment if this one has less than [`RED_ZONE`] left.
pub(crate) fn grow<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(RED_ZONE, SEGMENT, f)
}

/// A plan with this many relational steps or fewer runs on the caller's stack.
const SHALLOW: usize = 64;

/// The stack reserved per relational step of a longer plan. DataFusion's
/// optimizer and planner, and the drop of its plans, recurse once per level of
/// a plan, several frames at a time.
const PER_STEP: usize = 16 * 1024;

/// The most stack a plan may reserve: 1 GiB, about 65,000 relational steps.
const MAX_STACK: usize = 1 << 30;

#[cfg(test)]
thread_local! {
    /// Lowers [`MAX_STACK`] in unit tests.
    static TEST_MAX_STACK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn max_stack() -> usize {
    #[cfg(test)]
    if let Some(max) = TEST_MAX_STACK.with(|c| c.get()) {
        return max;
    }
    MAX_STACK
}

/// Run `f`, the `what` (`collect()`, `schema()`, ...) of a plan with `steps`
/// relational steps, on a stack it cannot overflow (#216).
///
/// Each relational step lowers to a DataFusion operator, and DataFusion walks
/// its plans recursively: a node frame filtered 10,000 times overflows an 8 MiB
/// stack in its optimizer. A plan that long runs on a fresh segment sized to
/// it; the pages it never touches are never committed. A plan of up to
/// [`SHALLOW`] steps, which is every plan written by hand, runs as before. A
/// plan that would need more than [`MAX_STACK`] is refused with an error that
/// says how long a chain can be.
pub(crate) fn for_steps<R>(
    what: &str,
    steps: usize,
    f: impl FnOnce() -> PlanResult<R>,
) -> PlanResult<R> {
    if steps <= SHALLOW {
        return f();
    }
    let size = steps.saturating_mul(PER_STEP).saturating_add(SEGMENT);
    if size > max_stack() {
        let most = (max_stack() - SEGMENT) / PER_STEP;
        return Err(PlanError::Limit(format!(
            "{what}: the frame's plan has {steps} relational steps, more than the {most} \
             one plan can run. Collect part of the chain and build the rest on its result."
        )));
    }
    stacker::grow(size, f)
}

/// JSON shorter than this parses on the caller's stack.
const SHORT_JSON: usize = 64 * 1024;

/// The stack reserved per byte of a longer JSON plan: serde buffers an
/// internally tagged enum's fields as it reads them, recursing once per level
/// of nesting, and a plan nests one level per step, every few dozen bytes.
const PER_JSON_BYTE: usize = 128;

/// Run `f`, a parse of `json`, on a stack its nesting cannot overflow (#216).
pub(crate) fn for_json<R>(json: &str, f: impl FnOnce() -> R) -> R {
    if json.len() <= SHORT_JSON {
        return grow(f);
    }
    stacker::grow(
        json.len()
            .saturating_mul(PER_JSON_BYTE)
            .saturating_add(SEGMENT),
        f,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_past_the_stack_cap_is_refused() {
        TEST_MAX_STACK.with(|c| c.set(Some(SEGMENT + 100 * PER_STEP)));
        assert_eq!(for_steps("collect()", 64, || Ok(1)).unwrap(), 1);
        assert_eq!(for_steps("collect()", 100, || Ok(2)).unwrap(), 2);
        let err = for_steps("collect()", 101, || Ok(3)).unwrap_err();
        assert!(matches!(err, PlanError::Limit(_)));
        assert_eq!(
            err.to_string(),
            "collect(): the frame's plan has 101 relational steps, more than the 100 one \
             plan can run. Collect part of the chain and build the rest on its result."
        );
        TEST_MAX_STACK.with(|c| c.set(None));
        assert_eq!(for_steps("collect()", 101, || Ok(4)).unwrap(), 4);
    }
}
