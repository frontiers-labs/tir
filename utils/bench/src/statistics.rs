//! Summary statistics of one benchmark's samples, after Akinshin, "Pro .NET
//! Benchmarking", chapter 4: quartiles, Tukey's fences, and modality from
//! Brendan Gregg's modal value over a histogram.

/// Distribution of one metric's samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distribution {
    pub median: f64,
    pub lower_quartile: f64,
    pub upper_quartile: f64,
    /// Samples outside Tukey's fences, 1.5 interquartile ranges past a quartile.
    pub outliers: usize,
    /// Gregg's modal value: 2 for one mode, about 4 for two.
    pub mvalue: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modality {
    Unimodal,
    Bimodal,
    Multimodal,
}

impl Modality {
    /// Akinshin's starting thresholds: up to 3.2 one mode, up to 4.2 two.
    pub fn of(mvalue: f64) -> Self {
        match mvalue {
            m if m <= 3.2 => Modality::Unimodal,
            m if m <= 4.2 => Modality::Bimodal,
            _ => Modality::Multimodal,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Modality::Unimodal => "unimodal",
            Modality::Bimodal => "bimodal",
            Modality::Multimodal => "multimodal",
        }
    }
}

impl Distribution {
    /// Describe `samples`, which must not be empty.
    pub fn of(samples: &[f64]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        let lower_quartile = quantile(&sorted, 0.25);
        let upper_quartile = quantile(&sorted, 0.75);
        let fence = 1.5 * (upper_quartile - lower_quartile);
        let inside =
            |value: &f64| (lower_quartile - fence..=upper_quartile + fence).contains(value);
        let kept: Vec<f64> = sorted.iter().copied().filter(inside).collect();
        Self {
            median: quantile(&sorted, 0.5),
            lower_quartile,
            upper_quartile,
            outliers: sorted.len() - kept.len(),
            mvalue: mvalue(&kept),
        }
    }
}

/// Standard error of the median as a share of it, from the median absolute
/// deviation so that outliers do not keep sampling going.
pub fn relative_error(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = quantile(&sorted, 0.5);
    let mut deviations: Vec<f64> = sorted.iter().map(|value| (value - median).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    // 1.4826 scales the MAD to a standard deviation and 1.2533 is the median's
    // efficiency loss against the mean, both for normal data.
    let error = 1.2533 * 1.4826 * quantile(&deviations, 0.5) / (sorted.len() as f64).sqrt();
    if median == 0.0 { 0.0 } else { error / median }
}

// Hyndman and Fan type 7.
fn quantile(sorted: &[f64], probability: f64) -> f64 {
    let position = probability * (sorted.len() - 1) as f64;
    let below = position.floor() as usize;
    let above = position.ceil() as usize;
    sorted[below] + (sorted[above] - sorted[below]) * (position - below as f64)
}

// Bins are half as wide as Scott's rule suggests, as Akinshin recommends. A
// histogram can split a mode or merge two depending on where its first bin
// starts, so a mode only counts when it shows at every one of several offsets.
fn mvalue(sorted: &[f64]) -> f64 {
    const OFFSETS: usize = 4;
    let count = sorted.len() as f64;
    let mean = sorted.iter().sum::<f64>() / count;
    let variance = sorted
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (count - 1.0).max(1.0);
    let width = 3.5 * variance.sqrt() / (2.0 * count.cbrt());
    if width <= 0.0 {
        return 2.0;
    }
    (0..OFFSETS)
        .map(|offset| {
            let start = sorted[0] - width * offset as f64 / OFFSETS as f64;
            let bins = ((sorted[sorted.len() - 1] - start) / width) as usize + 1;
            let mut counts = vec![0usize; bins];
            for value in sorted {
                counts[(((value - start) / width) as usize).min(bins - 1)] += 1;
            }
            modal_value(&counts)
        })
        .fold(f64::INFINITY, f64::min)
}

// The sum of rises and falls between neighbouring bins, counting the empty
// space on both sides, over the highest bin: 2 for a single peak.
fn modal_value(counts: &[usize]) -> f64 {
    let padded = [0]
        .iter()
        .chain(counts)
        .chain(&[0])
        .copied()
        .collect::<Vec<_>>();
    let elevation: usize = padded
        .windows(2)
        .map(|pair| pair[0].abs_diff(pair[1]))
        .sum();
    elevation as f64 / *counts.iter().max().expect("a histogram has a bin") as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    // Normal samples from a fixed xorshift sequence, by the Box-Muller transform.
    fn normal(center: f64, count: usize, seed: u64) -> Vec<f64> {
        let mut state = seed;
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..count)
            .map(|_| {
                let radius = (-2.0 * (1.0 - uniform()).ln()).sqrt();
                center + radius * (std::f64::consts::TAU * uniform()).cos()
            })
            .collect()
    }

    #[test]
    fn modes_are_counted_from_the_samples() {
        let one = normal(100.0, 100, 1);
        let two = [normal(100.0, 50, 2), normal(106.0, 50, 3)].concat();
        let three = [
            normal(100.0, 30, 4),
            normal(110.0, 30, 5),
            normal(120.0, 30, 6),
        ]
        .concat();
        assert_eq!(
            Modality::of(Distribution::of(&one).mvalue),
            Modality::Unimodal
        );
        assert_eq!(
            Modality::of(Distribution::of(&two).mvalue),
            Modality::Bimodal
        );
        assert_eq!(
            Modality::of(Distribution::of(&three).mvalue),
            Modality::Multimodal
        );
    }

    #[test]
    fn outliers_leave_the_quartiles_and_the_mode_alone() {
        let clean = normal(100.0, 40, 7);
        let samples = [clean.as_slice(), &[1000.0, 2000.0]].concat();
        let distribution = Distribution::of(&samples);
        assert!(distribution.outliers >= 2);
        assert_eq!(Modality::of(distribution.mvalue), Modality::Unimodal);
        assert!(distribution.upper_quartile < 104.0);
    }

    #[test]
    fn identical_samples_have_one_mode_and_no_error() {
        let distribution = Distribution::of(&[5.0; 30]);
        assert_eq!(distribution.mvalue, 2.0);
        assert_eq!(relative_error(&[5.0; 30]), 0.0);
    }
}
