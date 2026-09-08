//! A thin parallelism shim so `ursa-core` builds with **or** without `rayon`.
//!
//! The `rayon` feature is on by default, so the normal build re-exports rayon's
//! prelude and thread count unchanged — zero overhead, zero behaviour change. With
//! the feature off (the `wasm32-unknown-unknown` build, where threads need
//! `SharedArrayBuffer` + COOP/COEP headers a static host can't assume) this module
//! supplies **serial** equivalents with the *same call syntax*: `into_par_iter`,
//! `par_iter`, `par_iter_mut`, `par_chunks`, and `map_init` all exist as extension
//! methods that run on plain `std` iterators.
//!
//! Every serial equivalent preserves order — `into_par_iter().map(..).collect()` and
//! `into_iter().map(..).collect()` visit the same elements in the same order, and
//! `map_init`'s per-item scratch is reset the same way whether one worker or eight
//! touched it — so a kernel's output is **bit-for-bit identical** with the feature on
//! or off. A kernel therefore switches `use rayon::prelude::*;` to
//! `use crate::parallel::*;` and needs no other change.
//!
//! (Only `ursa-core` carries this shim. `ursa-plan` keeps rayon unconditionally — it
//! pulls in DataFusion and is never a wasm target, so the flag stops at the core
//! boundary.)

#[cfg(feature = "rayon")]
pub use rayon::prelude::*;

/// The size of the thread pool driving the parallel kernels — used to decide whether
/// a parallel build is worth its fixed cost. Without `rayon` there is no pool, so this
/// is `1` and every such dispatch takes the serial branch.
#[cfg(feature = "rayon")]
pub fn current_num_threads() -> usize {
    rayon::current_num_threads()
}

#[cfg(not(feature = "rayon"))]
pub fn current_num_threads() -> usize {
    1
}

// --- serial fallbacks (compiled only without `rayon`) -----------------------
//
// Each mirrors the rayon method the kernels call, delegating to the ordered `std`
// iterator so results are byte-identical to the parallel path.
#[cfg(not(feature = "rayon"))]
mod serial {
    /// `into_par_iter()` -> `into_iter()`. Covers ranges (`0..n`) and owned
    /// collections (`Vec<_>`) — anything `IntoIterator`.
    pub trait IntoParIterExt: IntoIterator + Sized {
        fn into_par_iter(self) -> Self::IntoIter {
            self.into_iter()
        }
    }
    impl<T: IntoIterator> IntoParIterExt for T {}

    /// `par_iter()` / `par_chunks(n)` -> `iter()` / `chunks(n)`, on any slice.
    pub trait ParSliceExt<T> {
        fn par_iter(&self) -> std::slice::Iter<'_, T>;
        fn par_chunks(&self, chunk_size: usize) -> std::slice::Chunks<'_, T>;
    }
    impl<T> ParSliceExt<T> for [T] {
        fn par_iter(&self) -> std::slice::Iter<'_, T> {
            self.iter()
        }
        fn par_chunks(&self, chunk_size: usize) -> std::slice::Chunks<'_, T> {
            self.chunks(chunk_size)
        }
    }

    /// `par_iter_mut()` -> `iter_mut()`, on any mutable slice.
    pub trait ParSliceMutExt<T> {
        fn par_iter_mut(&mut self) -> std::slice::IterMut<'_, T>;
    }
    impl<T> ParSliceMutExt<T> for [T] {
        fn par_iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
            self.iter_mut()
        }
    }

    /// `map_init(init, op)` — rayon reuses one scratch value per worker; serially there
    /// is one worker, so build the scratch once and thread it through an ordinary
    /// `map`. Order (and therefore output bytes) is unchanged.
    pub trait MapInitExt: Iterator + Sized {
        fn map_init<T, R, F>(self, init: impl FnOnce() -> T, mut op: F) -> impl Iterator<Item = R>
        where
            F: FnMut(&mut T, Self::Item) -> R,
        {
            let mut scratch = init();
            self.map(move |item| op(&mut scratch, item))
        }
    }
    impl<I: Iterator> MapInitExt for I {}
}

#[cfg(not(feature = "rayon"))]
pub use serial::*;
