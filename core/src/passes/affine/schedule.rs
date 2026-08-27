//! The schedule as a placement: which `(permutation, tiling)` pairs the
//! dependence vectors admit, and what each costs.
//!
//! Nothing here edits the IR. It reads an [`AffineView`] and hands the arranger
//! a domain and a cost function; what comes back is one candidate, which
//! `lower` then builds.
//!
//! # The cost
//!
//! Every placement is scored from the access forms alone. A cache of
//! [`CACHE_LINES`] lines holds the working set of the innermost loops as long as
//! it fits; the lines that set spans are fetched once per run of those loops,
//! and the runs are what the loops outside them count. That is the fetched
//! total, and it is what tiling changes: a band whose tiles fit is one fetch of
//! its data where the untiled loops fetched it every run. Placements that fetch
//! alike are told apart by the lines each iteration of the innermost loop
//! touches, which is what interchange changes: a unit stride runs a line to its
//! end, a row stride starts a line every iteration. Both numbers come from the
//! problem, never from running anything; both are monotone in the direction the
//! placement is meant to move them, and the identity is enumerated first, so a
//! nest is rebuilt only where the model says something is gained.

use tir_arrange::{CostFn, Domain, Placement, Problem, SlotId, WorkItem};

use crate::analysis::affine::{AffineView, Component, Dependence, Offset, Sign};

/// The tile sizes v1 considers. `1` is untiled. Every later entry needs a corpus
/// number in the change that adds it.
const TILES: [usize; 5] = [1, 8, 16, 32, 64];

/// How deep a nest is searched with tiling. The domain is `k!·5ᵏ` candidates, so
/// past this only permutations are considered.
const MAX_TILED_DEPTH: usize = 4;

/// Cache lines the model expects a working set to stay inside: 8 MB at 64-byte
/// lines. A knob, set from the corpus: with the code fcc emits today an
/// iteration costs some sixty instructions, and the hierarchy absorbs every
/// working set the corpus has — a 512³ matmul, 3 MB — faster than the loops a
/// tiling adds run (+17 % cycles there, +6 % at 64³). Tiling pays only past
/// what the whole hierarchy holds, and this is the first power of two above
/// the corpus.
const CACHE_LINES: i128 = 131_072;

/// The line size the model uses where the data layout declares none.
pub(super) const DEFAULT_LINE_BYTES: i64 = 64;

/// The trip count the cost model assumes for a loop whose bounds do not spell
/// one. A knob.
const NOMINAL_TRIP: i64 = 64;

/// Costs are integers, so the lines-per-iteration ratio is carried scaled.
const SCALE: i128 = 1 << 16;

/// One schedule: the order the loops run in, and how each is tiled.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Candidate {
    /// Position `p` runs the nest's original dimension `permutation[p]`.
    pub permutation: Vec<usize>,
    /// Iterations of dimension `d` per tile; `1` where it is not tiled.
    pub tiles: Vec<usize>,
}

impl Candidate {
    pub fn identity(depth: usize) -> Self {
        Self {
            permutation: (0..depth).collect(),
            tiles: vec![1; depth],
        }
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::identity(self.permutation.len())
    }

    /// The positions the tiled dimensions occupy, which have to be one band.
    pub fn band(&self) -> Option<(usize, usize)> {
        let tiled: Vec<usize> = (0..self.permutation.len())
            .filter(|&p| self.tiles[self.permutation[p]] > 1)
            .collect();
        let (&first, &last) = (tiled.first()?, tiled.last()?);
        (tiled.len() == last - first + 1).then_some((first, last + 1))
    }

    /// The dimension that is tiled where exactly one is.
    pub fn sole_tiled(&self) -> Option<usize> {
        match self
            .tiles
            .iter()
            .enumerate()
            .filter(|&(_, &tile)| tile > 1)
            .map(|(d, _)| d)
            .collect::<Vec<_>>()[..]
        {
            [only] => Some(only),
            _ => None,
        }
    }
}

/// One loop of the nest a candidate names.
#[derive(Clone, Copy)]
pub(super) enum Level {
    Plain(usize),
    TileOuter(usize),
    TileInner(usize),
}

/// The loops a candidate names, outermost first: everything above the tiled band
/// as it was, then the band's tile loops, then everything below it.
pub(super) fn levels(candidate: &Candidate) -> Vec<Level> {
    let depth = candidate.permutation.len();
    let (first, last) = candidate.band().unwrap_or((depth, depth));
    let dimension = |position: usize| candidate.permutation[position];
    (0..first)
        .map(|p| Level::Plain(dimension(p)))
        .chain((first..last).map(|p| Level::TileOuter(dimension(p))))
        .chain((first..last).map(|p| Level::TileInner(dimension(p))))
        .chain((last..depth).map(|p| Level::Plain(dimension(p))))
        .collect()
}

/// What the nest's pairs forbid: every direction vector some pair admits,
/// expanded so each component is one sign.
struct Constraints {
    vectors: Vec<Vec<i8>>,
    /// A pair the view could not decide forbids every reordering.
    undecided: bool,
}

/// Read the pairs. `Conditional` is not this pass's input — a versioned nest's
/// then-copy has no such pair — so it refuses here like `Unknown` does.
fn constraints(view: &AffineView) -> Constraints {
    let mut vectors = Vec::new();
    let mut undecided = view.opaque
        || !view.is_rectangular()
        || view
            .accesses
            .iter()
            .any(|access| matches!(access.offset, Offset::NonAffine));
    for pair in &view.pairs {
        match &pair.dependence {
            Dependence::Independent => {}
            Dependence::Distances(components) => vectors.extend(expand(components)),
            Dependence::Conditional(..) | Dependence::Unknown => undecided = true,
        }
    }
    Constraints { vectors, undecided }
}

/// Every lexicographically positive sign vector a distance vector stands for.
/// The zero vector orders nothing, and a negative one is the same dependence
/// read backwards, so neither is kept.
fn expand(components: &[Component]) -> Vec<Vec<i8>> {
    let mut all = vec![Vec::new()];
    for component in components {
        let signs: &[i8] = match component {
            Component::Distance(distance) => match distance.signum() {
                0 => &[0],
                1 => &[1],
                _ => &[-1],
            },
            Component::Direction(Sign::Positive) => &[1],
            Component::Direction(Sign::Negative) => &[-1],
            Component::Any => &[-1, 0, 1],
        };
        all = all
            .into_iter()
            .flat_map(|prefix| {
                signs.iter().map(move |&sign| {
                    let mut next = prefix.clone();
                    next.push(sign);
                    next
                })
            })
            .collect();
    }
    all.retain(|vector| lexicographically_positive(vector));
    all
}

fn lexicographically_positive(vector: &[i8]) -> bool {
    vector
        .iter()
        .find(|&&sign| sign != 0)
        .is_some_and(|&sign| sign > 0)
}

impl Constraints {
    /// Whether a candidate keeps every dependence pointing forwards, and tiles
    /// only a band no dependence runs backwards inside.
    fn admits(&self, candidate: &Candidate) -> bool {
        if self.undecided {
            return candidate.is_identity();
        }
        let band = candidate.band();
        if candidate.tiles.iter().any(|&tile| tile > 1) && band.is_none() {
            return false;
        }
        self.vectors.iter().all(|vector| {
            let permuted: Vec<i8> = candidate
                .permutation
                .iter()
                .map(|&dimension| vector[dimension])
                .collect();
            lexicographically_positive(&permuted)
                && band.is_none_or(|(first, last)| permuted[first..last].iter().all(|&s| s >= 0))
        })
    }
}

/// What the nest's accesses stride by in each dimension, and how long each
/// dimension runs: everything the cost model reads.
struct Locality {
    /// Per affine access, bytes per iteration of each dimension.
    strides: Vec<Vec<i64>>,
    trips: Vec<i64>,
    line: i64,
}

fn locality(view: &AffineView, line: i64) -> Locality {
    let depth = view.depth();
    Locality {
        strides: view
            .accesses
            .iter()
            .filter_map(|access| match &access.offset {
                Offset::Affine(form) => Some(
                    (0..depth)
                        .map(|d| form.counter_coefficient(d) as i64)
                        .collect(),
                ),
                Offset::NonAffine => None,
            })
            .collect(),
        trips: view
            .loops
            .iter()
            .map(|l| l.trip.map_or(NOMINAL_TRIP, |trip| trip as i64))
            .collect(),
        line,
    }
}

/// One loop of a placement as the model sees it: the dimension it walks, how
/// many of that dimension's iterations one of its own advances, and how many
/// times it runs.
struct Walk {
    dimension: usize,
    advance: i64,
    trip: i64,
}

impl Locality {
    fn cost(&self, candidate: &Candidate) -> i64 {
        let walks = self.walks(candidate);
        let mut first = walks.len() - 1;
        while first > 0 && self.lines(&walks[first - 1..]) <= CACHE_LINES {
            first -= 1;
        }
        let runs: i128 = walks[..first].iter().map(|w| i128::from(w.trip)).product();
        let fetched = runs.saturating_mul(self.lines(&walks[first..]));
        let innermost = walks.last().expect("a nest has a loop");
        let touched: i128 = self
            .strides
            .iter()
            .map(|strides| {
                self.contribution(
                    strides[innermost.dimension] * innermost.advance,
                    innermost.trip,
                )
            })
            .sum();
        let per_iteration = touched * SCALE / i128::from(innermost.trip.max(1));
        let tie = SCALE * (self.strides.len() as i128 + 1);
        fetched
            .saturating_mul(tie)
            .saturating_add(per_iteration)
            .min(i128::from(i64::MAX)) as i64
    }

    fn walks(&self, candidate: &Candidate) -> Vec<Walk> {
        levels(candidate)
            .into_iter()
            .map(|level| match level {
                Level::Plain(d) => Walk {
                    dimension: d,
                    advance: 1,
                    trip: self.trips[d],
                },
                Level::TileOuter(d) => Walk {
                    dimension: d,
                    advance: candidate.tiles[d] as i64,
                    trip: (self.trips[d] + candidate.tiles[d] as i64 - 1)
                        / candidate.tiles[d] as i64,
                },
                Level::TileInner(d) => Walk {
                    dimension: d,
                    advance: 1,
                    trip: candidate.tiles[d] as i64,
                },
            })
            .collect()
    }

    /// Distinct lines the accesses touch over one run of `walks`: per access,
    /// the product of what each loop contributes — nothing where the access does
    /// not move, one line per iteration where it moves by a line or more, and
    /// the fraction of a line it covers otherwise. That is exact for the
    /// row-major subscripts C spells.
    fn lines(&self, walks: &[Walk]) -> i128 {
        self.strides
            .iter()
            .map(|strides| {
                walks
                    .iter()
                    .map(|w| self.contribution(strides[w.dimension] * w.advance, w.trip))
                    .product::<i128>()
            })
            .sum()
    }

    fn contribution(&self, stride: i64, trip: i64) -> i128 {
        let (stride, trip, line) = (
            i128::from(stride.abs()),
            i128::from(trip),
            i128::from(self.line),
        );
        match stride {
            0 => 1,
            stride if stride >= line => trip,
            stride => (trip * stride / line).max(1),
        }
    }
}

/// Every candidate the nest admits, in a deterministic order: permutations
/// lexicographically, and tilings lexicographically inside each.
fn candidates(view: &AffineView, constraints: &Constraints) -> Vec<Candidate> {
    let depth = view.depth();
    let tilings: Vec<Vec<usize>> = if depth <= MAX_TILED_DEPTH {
        tilings(view, depth)
    } else {
        vec![vec![1; depth]]
    };
    permutations(depth)
        .into_iter()
        .flat_map(|permutation| {
            tilings.iter().map(move |tiles| Candidate {
                permutation: permutation.clone(),
                tiles: tiles.clone(),
            })
        })
        .filter(|candidate| constraints.admits(candidate) && remainder_admitted(view, candidate))
        .collect()
}

/// A tiled dimension the tiles do not cover whole leaves a remainder, and the
/// remainder is built by strip-mining that dimension's loop once the nest
/// stands — which is one loop, so only a lone tiled dimension may have one.
fn remainder_admitted(view: &AffineView, candidate: &Candidate) -> bool {
    let remainders = (0..view.depth())
        .filter(|&d| candidate.tiles[d] > 1 && !divides_evenly(view, d, candidate.tiles[d]))
        .count();
    remainders == 0 || candidate.sole_tiled().is_some()
}

/// Whether dimension `d` runs a whole number of tiles.
pub(super) fn divides_evenly(view: &AffineView, dimension: usize, tile: usize) -> bool {
    view.loops[dimension]
        .trip
        .is_some_and(|trip| trip % tile as i128 == 0)
}

/// Tiles worth trying: a size a dimension does not even reach is the whole loop
/// wrapped in one that runs once, which is overhead and no reuse.
fn tilings(view: &AffineView, depth: usize) -> Vec<Vec<usize>> {
    let mut all = vec![Vec::new()];
    for dimension in 0..depth {
        let trip = view.loops[dimension].trip;
        all = all
            .into_iter()
            .flat_map(|prefix| {
                TILES
                    .iter()
                    .filter(|&&tile| tile == 1 || trip.is_none_or(|trip| (tile as i128) < trip))
                    .map(move |&tile| {
                        let mut next = prefix.clone();
                        next.push(tile);
                        next
                    })
            })
            .collect();
    }
    all
}

fn permutations(depth: usize) -> Vec<Vec<usize>> {
    let mut all = vec![Vec::new()];
    for _ in 0..depth {
        all = all
            .into_iter()
            .flat_map(|prefix| {
                (0..depth)
                    .filter(|dimension| !prefix.contains(dimension))
                    .map(|dimension| {
                        let mut next = prefix.clone();
                        next.push(dimension);
                        next
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    all
}

/// The schedule of least modelled cost, or the identity where the arranger
/// cannot decide.
pub(super) fn schedule(view: &AffineView, line: i64) -> Candidate {
    let constraints = constraints(view);
    let locality = locality(view, line);
    let candidates = candidates(view, &constraints);
    if candidates.is_empty() {
        return Candidate::identity(view.depth());
    }
    let scored: Vec<i64> = candidates.iter().map(|c| locality.cost(c)).collect();
    let cost: CostFn = Box::new(move |placement: &Placement| scored[placement[0]]);
    let problem = Problem {
        work: vec![WorkItem { id: 0 }],
        domain: vec![Domain {
            item: 0,
            slots: (0..candidates.len()).collect::<Vec<SlotId>>(),
        }],
        precedence: Vec::new(),
        capacity: Vec::new(),
        cost,
    };
    match tir_arrange::solve(&problem) {
        Some(placement) => candidates[placement[0]].clone(),
        None => Candidate::identity(view.depth()),
    }
}
