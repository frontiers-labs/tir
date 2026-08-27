//! The single-equation dependence test.
//!
//! Two accesses of one object collide when their byte ranges meet:
//! `[f(i), f(i)+eₐ)` and `[g(j), g(j)+e_b)` share a byte exactly when
//! `f(i) - g(j) ∈ (-eₐ, e_b)`. Where the two forms agree on every coefficient —
//! the only shape v1 reads — that difference is `Δk - Σ a_d·δ_d` in the distance
//! `δ = j - i`, so the whole question is one linear equation over `δ` with the
//! iteration space as its box.
//!
//! The answer is the set of distances the equation admits, summarized per depth.
//! It is enumerated over direction vectors: each of `{<, =, >}ᵏ` fixes a box for
//! `δ`, and a box is admitted when the sum's Banerjee bounds reach the target
//! interval and the GCD of the free coefficients divides something inside it.
//! Both are necessary conditions only, so a box the pair cannot actually realize
//! may survive — a dependence too many, never one too few.

/// Which way a dependence runs at one depth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sign {
    Positive,
    Negative,
}

/// What is provable about the distance at one depth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Component {
    /// The one distance the pair admits.
    Distance(i128),
    /// Only the direction is provable.
    Direction(Sign),
    /// Any distance the space holds.
    Any,
}

/// A bound wide enough that no iteration space reaches it, standing in for a
/// trip count the view cannot read. Coefficients are byte strides, so the
/// products below stay far inside `i128`.
const UNBOUNDED: i128 = 1 << 62;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Zero,
    Positive,
    Negative,
}

/// Every distance `Σ coefficients[d]·δ_d ∈ targets` admits, summarized per
/// depth, or `None` where it admits none.
///
/// `extents[d]` is the greatest distance depth `d` can span — one less than its
/// trip count — and `None` where the trip count is not known. Only
/// lexicographically positive distances are enumerated: the zero distance orders
/// nothing, and a negative one is the same dependence read backwards.
pub fn distances(
    coefficients: &[i128],
    extents: &[Option<i128>],
    targets: &[(i128, i128)],
) -> Option<Vec<Component>> {
    let depth = coefficients.len();
    let extents: Vec<i128> = extents.iter().map(|e| e.unwrap_or(UNBOUNDED)).collect();
    let admitted: Vec<Vec<Direction>> = enumerate(depth)
        .into_iter()
        .filter(|directions| admits(coefficients, &extents, targets, directions))
        .collect();
    if admitted.is_empty() {
        return None;
    }
    Some(
        (0..depth)
            .map(|d| summarize(coefficients, &extents, targets, &admitted, d))
            .collect(),
    )
}

/// Every lexicographically positive direction vector of `depth` components.
fn enumerate(depth: usize) -> Vec<Vec<Direction>> {
    let mut all = vec![Vec::new()];
    for _ in 0..depth {
        all = all
            .into_iter()
            .flat_map(|prefix| {
                [Direction::Zero, Direction::Positive, Direction::Negative]
                    .into_iter()
                    .map(move |direction| {
                        let mut next = prefix.clone();
                        next.push(direction);
                        next
                    })
            })
            .collect();
    }
    all.retain(|directions| {
        directions
            .iter()
            .find(|&&d| d != Direction::Zero)
            .is_some_and(|&d| d == Direction::Positive)
    });
    all
}

/// The distances one direction admits at one depth.
fn span(extents: &[i128], directions: &[Direction], depth: usize) -> (i128, i128) {
    match directions[depth] {
        Direction::Zero => (0, 0),
        Direction::Positive => (1, extents[depth]),
        Direction::Negative => (-extents[depth], -1),
    }
}

/// Whether the equation can hold anywhere in the box a direction vector fixes.
fn admits(
    coefficients: &[i128],
    extents: &[i128],
    targets: &[(i128, i128)],
    directions: &[Direction],
) -> bool {
    let (low, high) = sum_range(coefficients, extents, directions, None);
    let divisor = coefficients
        .iter()
        .enumerate()
        .filter(|&(d, _)| directions[d] != Direction::Zero)
        .fold(0, |acc, (_, &c)| gcd(acc, c));
    targets.iter().any(|&(first, last)| {
        let (first, last) = (first.max(low), last.min(high));
        first <= last && divides_something(divisor, first, last)
    })
}

/// The values `Σ coefficients[d]·δ_d` reaches over the box, leaving `skip` out.
fn sum_range(
    coefficients: &[i128],
    extents: &[i128],
    directions: &[Direction],
    skip: Option<usize>,
) -> (i128, i128) {
    let (mut low, mut high) = (0, 0);
    for (depth, &coefficient) in coefficients.iter().enumerate() {
        if skip == Some(depth) {
            continue;
        }
        let (first, last) = span(extents, directions, depth);
        let (a, b) = (coefficient * first, coefficient * last);
        low += a.min(b);
        high += a.max(b);
    }
    (low, high)
}

/// Whether some multiple of `divisor` lies in `[low, high]`. A divisor of zero
/// leaves the sum pinned at zero.
fn divides_something(divisor: i128, low: i128, high: i128) -> bool {
    if divisor == 0 {
        return low <= 0 && 0 <= high;
    }
    high.div_euclid(divisor) * divisor >= low
}

fn gcd(a: i128, b: i128) -> i128 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// What the admitted directions leave provable about depth `d`.
fn summarize(
    coefficients: &[i128],
    extents: &[i128],
    targets: &[(i128, i128)],
    admitted: &[Vec<Direction>],
    depth: usize,
) -> Component {
    let mut low = i128::MAX;
    let mut high = i128::MIN;
    for directions in admitted {
        let (first, last) = reachable(coefficients, extents, targets, directions, depth);
        low = low.min(first);
        high = high.max(last);
    }
    match () {
        _ if low == high => Component::Distance(low),
        _ if low > 0 => Component::Direction(Sign::Positive),
        _ if high < 0 => Component::Direction(Sign::Negative),
        _ => Component::Any,
    }
}

/// The distances depth `d` can take inside one direction's box, once the
/// equation has narrowed them.
fn reachable(
    coefficients: &[i128],
    extents: &[i128],
    targets: &[(i128, i128)],
    directions: &[Direction],
    depth: usize,
) -> (i128, i128) {
    let (first, last) = span(extents, directions, depth);
    let coefficient = coefficients[depth];
    if coefficient == 0 {
        return (first, last);
    }
    let (rest_low, rest_high) = sum_range(coefficients, extents, directions, Some(depth));
    let (mut low, mut high) = (i128::MAX, i128::MIN);
    for &(target_low, target_high) in targets {
        // `coefficient · δ` must land in what the other depths leave of the target,
        // and `δ` is whole, so the interval rounds inward.
        let (product_low, product_high) = (target_low - rest_high, target_high - rest_low);
        let (a, b) = if coefficient > 0 {
            (
                ceil_div(product_low, coefficient),
                floor_div(product_high, coefficient),
            )
        } else {
            (
                ceil_div(product_high, coefficient),
                floor_div(product_low, coefficient),
            )
        };
        let (a, b) = (a.max(first), b.min(last));
        if a <= b {
            low = low.min(a);
            high = high.max(b);
        }
    }
    if low > high {
        (first, last)
    } else {
        (low, high)
    }
}

fn floor_div(a: i128, b: i128) -> i128 {
    if b < 0 {
        (-a).div_euclid(-b)
    } else {
        a.div_euclid(b)
    }
}

fn ceil_div(a: i128, b: i128) -> i128 {
    -floor_div(-a, b)
}
