//! The frame budget: how much to draw while the view moves.
//!
//! A frame has 16 ms, and a million edges cost an integrated GPU more than
//! that and a software renderer seconds. So while the camera or the
//! positions change, the renderer draws less: a prefix of the edge buffer,
//! which is in importance order ([`crate::order`]), and, if even the nodes
//! are too many, the largest of them. Once the view has been at rest for
//! [`REST_MS`] it draws everything again. [`FrameBudget`] finds how much:
//! it follows what the last frames cost, so each device settles at its own
//! level without being told what it is.
//!
//! # Units
//!
//! The budget is in units of one edge: two vertices, which is what an edge
//! costs to draw where drawing is slow. A node is a quad of six, so three
//! units ([`NODE_UNITS`]). [`split`] shares a budget between the two: half
//! to the nodes until they are all drawn, the rest to the edges. Everything
//! is `edges + 3 * nodes` units ([`total_units`]).
//!
//! # What a frame costs
//!
//! The binding measures it and says ([`FrameBudget::observe`]): the time
//! from a frame's start to when the next one could start. In a browser that
//! is the time to the next animation frame, which is a whole number of the
//! display's refresh periods: a frame that fits costs one period however
//! much room it had left, and one that does not costs two. So the cost says
//! whether a budget fits, never by how much, and the controller searches:
//!
//! - It holds the largest budget known to fit, and now and then tries a
//!   larger one. If that holds for [`HOLD`] frames it is the new budget, and
//!   the next try is bolder (4%, 8%, … up to 50% more); if it misses, the
//!   budget goes back, the tries get finer down to 4%, and then rarer (every
//!   30 frames, then 60, … up to 480). Under a steady load the budget moves
//!   by 4%, with one late frame for each try.
//! - A budget that held and now misses twice running is too much for what
//!   is drawn now (the user zoomed in; something else took the GPU): it is
//!   cut in proportion to the cost. Once is a hiccup, and changes nothing.
//!
//! A frame fits if it costs no more than [`TARGET_MS`], or, where the
//! display's period is longer than that, 1.3 periods (22 ms at 60 Hz: one
//! period, with room for the timer's jitter). A device that cannot show
//! even the least budget ([`MIN_UNITS`]) in that time stays at the least: a
//! software renderer takes 20 to 30 ms to show a canvas with next to
//! nothing on it, and draws 170 nodes and 512 edges of a million while it
//! moves.
//!
//! A new view starts at [`FIRST_UNITS`] and doubles its budget every frame
//! that fits, until one does not: a million edges by the seventh frame, if
//! they fit, and one late frame to find out where they do not.

/// The cost a moving frame is held to, in milliseconds.
pub const TARGET_MS: f64 = 12.0;
/// How long after the last movement the view is at rest, in milliseconds:
/// everything is drawn again.
pub const REST_MS: f64 = 200.0;
/// The budget a new view starts with, in units.
pub const FIRST_UNITS: usize = 1 << 14;
/// The least budget: a moving frame never draws less than this, or than
/// there is.
pub const MIN_UNITS: usize = 1 << 10;
/// What a node costs, in units of one edge.
pub const NODE_UNITS: usize = 3;
/// How many frames a larger prefix must fit for before it is kept.
pub const HOLD: u32 = 3;
/// How many frame costs the p95 is over.
pub const WINDOW: usize = 120;

/// The finest and the boldest try, as a fraction more.
const FINE: f64 = 0.04;
const BOLD: f64 = 0.5;
/// Frames between tries once they are fine, at first and at most.
const PATIENCE: u32 = 30;
const MAX_PATIENCE: u32 = 480;
/// The display's refresh period, assumed until a shorter frame is seen: 60 Hz.
const PERIOD_MS: f64 = 1000.0 / 60.0;

/// Everything there is to draw, in units: `edges` edges and `nodes` nodes.
pub fn total_units(nodes: usize, edges: usize) -> usize {
    edges + NODE_UNITS * nodes
}

/// How many of `nodes` nodes and `edges` edges a budget of `units` draws:
/// `(nodes, edges)`. Half the budget goes to the nodes, until they are all
/// drawn, and the rest to the edges; what the edges do not need goes back
/// to the nodes. Never none of either, if there are any.
pub fn split(units: usize, nodes: usize, edges: usize) -> (usize, usize) {
    if units >= total_units(nodes, edges) {
        return (nodes, edges);
    }
    let half = units / 2;
    let n = (half / NODE_UNITS).max(1).min(nodes);
    let rest = if n == nodes {
        units.saturating_sub(NODE_UNITS * n)
    } else {
        units - half
    };
    let m = rest.max(1).min(edges);
    let spare = units.saturating_sub(m) / NODE_UNITS;
    (n.max(spare.min(nodes)), m)
}

/// What a frame draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// How much, in units ([`split`] says of what).
    pub units: usize,
    /// Whether the view is moving: the binding measures this frame's cost
    /// and reports it ([`FrameBudget::observe`]).
    pub moving: bool,
    /// Whether anything was left out: the binding draws again after
    /// [`REST_MS`] without movement.
    pub partial: bool,
}

/// The controller. See the module docs.
#[derive(Debug, Clone)]
pub struct FrameBudget {
    /// The prefix known to fit.
    fits: f64,
    /// A larger prefix being tried, and how many frames it has fitted for.
    trying: Option<(f64, u32)>,
    /// How much more the next try is.
    step: f64,
    /// A try has missed since the tries were last fine: they get finer,
    /// not bolder.
    narrowing: bool,
    /// No frame has missed yet: the budget doubles every frame.
    starting: bool,
    /// Frames to fit before the next try, and how many have.
    patience: u32,
    fitted: u32,
    /// The held prefix missed on the last frame.
    struck: bool,
    /// The shortest frame seen: the display's refresh period, if shorter
    /// than 60 Hz's.
    period: f64,
    /// When the view last moved, and whether it has since the last frame.
    last_move: f64,
    moved: bool,
    /// The prefix the last moving frame drew, of how many edges.
    drawn: (usize, usize),
    /// The last [`WINDOW`] costs.
    costs: Vec<f32>,
    observed: usize,
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameBudget {
    pub fn new() -> Self {
        Self {
            fits: FIRST_UNITS as f64,
            trying: None,
            step: 1.0,
            narrowing: false,
            starting: true,
            patience: 0,
            fitted: 0,
            struck: false,
            period: PERIOD_MS,
            last_move: f64::NEG_INFINITY,
            moved: false,
            drawn: (0, 0),
            costs: Vec::new(),
            observed: 0,
        }
    }

    /// The camera or the positions changed: the next frame is a moving one.
    pub fn moved(&mut self) {
        self.moved = true;
    }

    /// What the frame at `now_ms` (any clock in milliseconds) draws of
    /// `total` units: the budget if the view moved since the last frame or
    /// less than [`REST_MS`] ago, else all of them.
    pub fn frame(&mut self, now_ms: f64, total: usize) -> Frame {
        let edges = total;
        if std::mem::take(&mut self.moved) {
            self.last_move = now_ms;
        }
        let moving = now_ms - self.last_move < REST_MS;
        if !moving {
            return Frame {
                units: edges,
                moving,
                partial: false,
            };
        }
        let k = self.prefix(edges);
        self.drawn = (k, edges);
        Frame {
            units: k,
            moving,
            partial: k < edges,
        }
    }

    /// The budget a moving frame has of `edges` units now: never none of
    /// them.
    pub fn prefix(&self, edges: usize) -> usize {
        let k = self.trying.map_or(self.fits, |t| t.0);
        (k as usize).max(MIN_UNITS).min(edges)
    }

    /// The cost a frame is held to: [`TARGET_MS`], or 1.3 of the display's
    /// refresh periods if that is more.
    pub fn limit_ms(&self) -> f64 {
        TARGET_MS.max(1.3 * self.period)
    }

    /// The last moving frame cost `cost_ms`: from its start to when the
    /// next frame could start.
    pub fn observe(&mut self, cost_ms: f64) {
        if !cost_ms.is_finite() || cost_ms <= 0.0 {
            return;
        }
        if self.costs.len() < WINDOW {
            self.costs.push(cost_ms as f32);
        } else {
            self.costs[self.observed % WINDOW] = cost_ms as f32;
        }
        self.observed += 1;
        self.period = self.period.min(cost_ms.max(8.0));
        let (drawn, edges) = self.drawn;
        let all = drawn >= edges;
        let fit = cost_ms <= self.limit_ms();
        let hold = if self.starting { 1 } else { HOLD };
        if !fit && std::mem::take(&mut self.starting) {
            self.step = BOLD;
        }
        match (self.trying.take(), fit) {
            // The try fits so far; kept after HOLD frames, and the next is
            // bolder unless a try has missed.
            (Some((k, held)), true) => {
                if held + 1 < hold {
                    self.trying = Some((k, held + 1));
                } else {
                    self.fits = k;
                    if !self.narrowing && !self.starting {
                        self.step = (2.0 * self.step).min(BOLD);
                    }
                    self.fitted = self.patience;
                }
            }
            // The try missed: back, and finer; once fine, rarer.
            (Some(_), false) => {
                self.narrowing = true;
                if self.step > FINE {
                    self.step = (self.step / 2.0).max(FINE);
                    self.fitted = self.patience;
                } else {
                    self.patience = (2 * self.patience).clamp(PATIENCE, MAX_PATIENCE);
                    self.fitted = 0;
                }
            }
            (None, true) => {
                self.struck = false;
                self.fitted = self.fitted.saturating_add(1);
            }
            // What fitted does not: once is a hiccup, twice is the load.
            (None, false) => {
                if std::mem::replace(&mut self.struck, true) {
                    let cut = (0.95 * self.limit_ms() / cost_ms).clamp(0.25, 0.93);
                    self.fits = (drawn as f64 * cut).max(MIN_UNITS as f64);
                    self.struck = false;
                    self.step = FINE;
                    self.narrowing = false;
                    self.patience = PATIENCE;
                    self.fitted = 0;
                }
            }
        }
        // Time for a try, if there is more to draw.
        if self.trying.is_none() && !self.struck && !all && self.fitted >= self.patience {
            if self.step <= FINE && self.narrowing {
                // The fine tries start again, and may get bolder.
                self.narrowing = false;
            }
            self.trying = Some((self.fits.max(MIN_UNITS as f64) * (1.0 + self.step), 0));
        }
        if all {
            // Everything is drawn: nothing larger to try, or to remember.
            self.fits = self.fits.min(edges.max(MIN_UNITS) as f64);
        }
    }

    /// The units the last moving frame drew, and of how many; `(0, 0)`
    /// before any.
    pub fn drawn(&self) -> (usize, usize) {
        self.drawn
    }

    /// How many frame costs have been observed.
    pub fn observed(&self) -> usize {
        self.observed
    }

    /// The 95th percentile of the last [`WINDOW`] costs (nearest rank), in
    /// milliseconds; `None` before any.
    pub fn p95_ms(&self) -> Option<f32> {
        let mut sorted = self.costs.clone();
        sorted.sort_by(f32::total_cmp);
        let rank = (0.95 * sorted.len() as f64).ceil() as usize;
        sorted.get(rank.saturating_sub(1)).copied()
    }
}

/// The opacity to draw `drawn` of `edges` edges with, where all of them are
/// drawn with `alpha`: more, so that where edges pile up the ink is as deep
/// as with all of them (`1 - (1 - alpha)^(edges / drawn)`: `edges / drawn`
/// layers in one), up to 0.6 so that a short prefix is still lines the
/// nodes show through.
pub fn prefix_alpha(alpha: f32, drawn: usize, edges: usize) -> f32 {
    if drawn >= edges || drawn == 0 {
        return alpha;
    }
    let layers = edges as f32 / drawn as f32;
    let deep = 1.0 - (1.0 - alpha.clamp(0.0, 1.0)).powf(layers);
    deep.min(alpha.max(0.6))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device: a frame of `k` edges costs `fixed + per_edge * k` ms, and
    /// is shown at the next refresh if `period` is given.
    #[derive(Clone, Copy)]
    struct Device {
        fixed: f64,
        per_edge: f64,
        period: Option<f64>,
    }

    impl Device {
        fn cost(&self, k: usize) -> f64 {
            let raw = self.fixed + self.per_edge * k as f64;
            match self.period {
                Some(p) => p * (raw / p).ceil().max(1.0),
                None => raw.max(PERIOD_MS),
            }
        }

        /// The longest prefix that fits.
        fn fits(&self, limit: f64, edges: usize) -> usize {
            let within = self.period.unwrap_or(limit);
            (((within - self.fixed) / self.per_edge) as usize).min(edges)
        }
    }

    /// Run `frames` moving frames, 16 ms apart; the prefix of each.
    fn run(budget: &mut FrameBudget, device: Device, edges: usize, frames: usize) -> Vec<usize> {
        (0..frames)
            .map(|i| {
                budget.moved();
                let frame = budget.frame(16.0 * i as f64, edges);
                assert!(frame.moving);
                budget.observe(device.cost(frame.units));
                frame.units
            })
            .collect()
    }

    const M: usize = 1_000_000;

    fn devices() -> Vec<(&'static str, Device)> {
        let hz60 = Some(1000.0 / 60.0);
        vec![
            // Fits 200K edges in a 60 Hz frame.
            (
                "integrated GPU",
                Device {
                    fixed: 2.0,
                    per_edge: 14.6 / 200_000.0,
                    period: hz60,
                },
            ),
            // 3.9 s for a million edges, and its frames end when they end.
            (
                "software",
                Device {
                    fixed: 6.0,
                    per_edge: 3.9e-3,
                    period: None,
                },
            ),
            // Software, shown at the next refresh.
            (
                "software, vsynced",
                Device {
                    fixed: 6.0,
                    per_edge: 3.9e-3,
                    period: hz60,
                },
            ),
            // 120 Hz, and room for 400K.
            (
                "120 Hz",
                Device {
                    fixed: 1.0,
                    per_edge: 7.3 / 400_000.0,
                    period: Some(1000.0 / 120.0),
                },
            ),
        ]
    }

    #[test]
    fn it_converges_on_what_fits() {
        for (name, device) in devices() {
            let mut budget = FrameBudget::new();
            let ks = run(&mut budget, device, M, 600);
            let best = device.fits(budget.limit_ms(), M);
            let settled = &ks[300..];
            let (lo, hi) = (
                *settled.iter().min().unwrap(),
                *settled.iter().max().unwrap(),
            );
            assert!(
                lo as f64 >= 0.85 * best as f64 && hi as f64 <= 1.05 * best as f64,
                "{name}: {lo}..{hi} of {best}"
            );
            // And soon: within 10% of where it settles in 120 frames.
            let soon = ks[120] as f64;
            assert!(soon >= 0.85 * best as f64, "{name}: {soon} at 120");
        }
    }

    #[test]
    fn it_never_draws_no_edges() {
        // A device that cannot draw anything in time.
        let hopeless = Device {
            fixed: 80.0,
            per_edge: 1e-3,
            period: None,
        };
        let mut budget = FrameBudget::new();
        let ks = run(&mut budget, hopeless, M, 300);
        assert!(ks.iter().all(|&k| k >= MIN_UNITS), "{:?}", &ks[..10]);
        assert_eq!(*ks.last().unwrap(), MIN_UNITS);
        // A graph smaller than the least prefix is drawn whole, and one
        // edge is one edge.
        for edges in [1, 5, MIN_UNITS - 1] {
            let ks = run(&mut FrameBudget::new(), hopeless, edges, 50);
            assert!(ks.iter().all(|&k| k == edges), "{edges}: {ks:?}");
        }
        // No edges: none to draw.
        assert_eq!(FrameBudget::new().prefix(0), 0);
    }

    #[test]
    fn under_a_steady_load_it_moves_by_less_than_a_tenth() {
        for (name, device) in devices() {
            let mut budget = FrameBudget::new();
            let ks = run(&mut budget, device, M, 3000);
            let settled = &ks[400..];
            let (lo, hi) = (
                *settled.iter().min().unwrap() as f64,
                *settled.iter().max().unwrap() as f64,
            );
            assert!(hi <= 1.10 * lo, "{name}: {lo}..{hi}");
            // The tries are few: 19 frames in 20 fit.
            let limit = budget.limit_ms();
            let late = settled.iter().filter(|&&k| device.cost(k) > limit).count();
            assert!(20 * late <= settled.len(), "{name}: {late} late");
            assert!(budget.p95_ms().unwrap() as f64 <= limit, "{name}");
        }
    }

    #[test]
    fn a_fast_device_draws_everything() {
        let fast = Device {
            fixed: 1.0,
            per_edge: 2e-6,
            period: Some(1000.0 / 60.0),
        };
        let mut budget = FrameBudget::new();
        let ks = run(&mut budget, fast, M, 120);
        assert_eq!(*ks.last().unwrap(), M);
        // From the first prefix to all of them in under a second.
        assert!(ks[50] == M, "{}", ks[50]);
        let frame = budget.frame(16.0 * 121.0, M);
        assert!(!frame.partial);
    }

    #[test]
    fn it_follows_a_load_that_changes() {
        let (_, mut device) = devices().remove(0);
        let mut budget = FrameBudget::new();
        run(&mut budget, device, M, 400);
        // Three times the cost an edge (the user zoomed in): cut within a
        // few frames.
        device.per_edge *= 3.0;
        let ks = run(&mut budget, device, M, 400);
        let best = device.fits(budget.limit_ms(), M) as f64;
        assert!(ks[10] as f64 <= 1.05 * best, "{} of {best}", ks[10]);
        assert!(*ks.last().unwrap() as f64 >= 0.85 * best);
        // And back: the prefix grows again, within a few seconds.
        device.per_edge /= 3.0;
        let ks = run(&mut budget, device, M, 400);
        let best = device.fits(budget.limit_ms(), M) as f64;
        assert!(ks[300] as f64 >= 0.85 * best, "{} of {best}", ks[300]);
    }

    #[test]
    fn one_slow_frame_changes_nothing() {
        let (_, device) = devices().remove(0);
        let mut budget = FrameBudget::new();
        let ks = run(&mut budget, device, M, 400);
        let before = *ks.last().unwrap();
        budget.moved();
        let frame = budget.frame(0.0, M);
        budget.observe(180.0);
        let after = run(&mut budget, device, M, 5);
        assert!(after[0] >= frame.units.min(before) * 96 / 100, "{after:?}");
    }

    #[test]
    fn at_rest_everything_is_drawn() {
        let mut budget = FrameBudget::new();
        // Nothing moved: all of them, and nothing to measure.
        let frame = budget.frame(1000.0, M);
        assert_eq!(
            (frame.units, frame.moving, frame.partial),
            (M, false, false)
        );
        budget.moved();
        let frame = budget.frame(2000.0, M);
        assert_eq!(
            (frame.units, frame.moving, frame.partial),
            (FIRST_UNITS, true, true)
        );
        assert_eq!(budget.drawn(), (FIRST_UNITS, M));
        // A frame soon after (a hover) is still a prefix; one after the
        // rest is everything.
        assert!(budget.frame(2000.0 + REST_MS - 1.0, M).partial);
        let frame = budget.frame(2000.0 + REST_MS, M);
        assert_eq!((frame.units, frame.moving), (M, false));
    }

    #[test]
    fn the_p95_is_of_the_last_frames() {
        let mut budget = FrameBudget::new();
        assert_eq!(budget.p95_ms(), None);
        for i in 0..100 {
            budget.observe(if i < 96 { 16.0 } else { 40.0 });
        }
        assert_eq!(budget.p95_ms(), Some(16.0));
        budget.observe(40.0);
        budget.observe(40.0);
        assert_eq!(budget.p95_ms(), Some(40.0));
        for _ in 0..WINDOW {
            budget.observe(17.0);
        }
        assert_eq!(budget.p95_ms(), Some(17.0));
        assert_eq!(budget.observed(), 102 + WINDOW);
    }

    #[test]
    fn a_budget_is_split_between_nodes_and_edges() {
        // Everything fits: everything.
        assert_eq!(split(total_units(100, 500), 100, 500), (100, 500));
        assert_eq!(split(usize::MAX, 100, 500), (100, 500));
        // Half the units to the nodes, three each; the rest to the edges.
        assert_eq!(split(6000, 131_072, 1_000_000), (1000, 3000));
        // All the nodes need less than half: the edges get the rest.
        assert_eq!(split(6000, 100, 1_000_000), (100, 5700));
        // Few edges: the nodes get what they leave.
        assert_eq!(split(6000, 131_072, 30), (1990, 30));
        // Never none of either.
        assert_eq!(split(1, 10, 10), (1, 1));
        assert_eq!(split(0, 10, 10), (1, 1));
        assert_eq!(split(MIN_UNITS, 131_072, 1_000_000), (170, 512));
        // None to draw.
        assert_eq!(split(100, 0, 0), (0, 0));
        assert_eq!(split(100, 0, 500), (0, 100));
        assert_eq!(split(100, 500, 0), (33, 0));
        // More units never draw less of either.
        let mut last = (0, 0);
        for units in 0..3000 {
            let now = split(units, 400, 1500);
            assert!(now.0 >= last.0 && now.1 >= last.1, "{units}: {now:?}");
            last = now;
        }
    }

    #[test]
    fn a_prefix_is_drawn_deeper() {
        // All of them: as they are.
        assert_eq!(prefix_alpha(0.05, 100, 100), 0.05);
        // Half: two layers in one.
        let half = prefix_alpha(0.05, 50, 100);
        assert!((half - (1.0 - 0.95f32 * 0.95)).abs() < 1e-6, "{half}");
        // A hundredth: no deeper than 0.6.
        assert_eq!(prefix_alpha(0.05, 1, 100), 0.6);
        // Never lighter than all of them are.
        assert_eq!(prefix_alpha(0.8, 1, 100), 0.8);
        assert_eq!(prefix_alpha(0.05, 0, 0), 0.05);
    }
}
