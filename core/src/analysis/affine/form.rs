//! Affine forms over a nest's counters and symbols.
//!
//! A form is `Σ cᵈ·dᵈ + Σ sⁱ·sⁱ + k`: dense coefficients over the loop counters
//! (outermost first) and over the values the nest was entered with, plus a
//! constant. Coefficients are `i128` so every combination a source width can
//! spell is exact; whether the source width could reach the value the form
//! names is [`AffineForm::fits`]'s question, asked separately.

/// One affine expression over a nest's iteration space.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct AffineForm {
    counters: Vec<i128>,
    symbols: Vec<i128>,
    constant: i128,
}

impl AffineForm {
    /// The form naming `value`.
    pub fn constant(value: i128) -> Self {
        Self {
            constant: value,
            ..Self::default()
        }
    }

    /// The form naming the counter of loop depth `depth`.
    pub fn counter(depth: usize) -> Self {
        let mut form = Self::default();
        form.grow_counters(depth + 1);
        form.counters[depth] = 1;
        form
    }

    /// The form naming symbol `index`.
    pub fn symbol(index: usize) -> Self {
        let mut form = Self::default();
        form.grow_symbols(index + 1);
        form.symbols[index] = 1;
        form
    }

    pub fn constant_term(&self) -> i128 {
        self.constant
    }

    pub fn counter_coefficient(&self, depth: usize) -> i128 {
        self.counters.get(depth).copied().unwrap_or(0)
    }

    pub fn symbol_coefficient(&self, index: usize) -> i128 {
        self.symbols.get(index).copied().unwrap_or(0)
    }

    /// The value the form names where it names one — no counter and no symbol
    /// contributes.
    pub fn as_constant(&self) -> Option<i128> {
        self.is_constant().then_some(self.constant)
    }

    pub fn is_constant(&self) -> bool {
        self.counters.iter().chain(&self.symbols).all(|&c| c == 0)
    }

    /// Whether no counter contributes: a form the nest's parameters alone decide.
    pub fn is_uniform(&self) -> bool {
        self.counters.iter().all(|&c| c == 0)
    }

    /// Whether the two forms differ in nothing but their constant.
    pub fn same_slopes(&self, other: &Self) -> bool {
        self.sub(other).is_constant()
    }

    /// Replace the coefficient of counter `depth`, growing the form to reach it.
    pub fn set_counter_coefficient(&mut self, depth: usize, value: i128) {
        self.grow_counters(depth + 1);
        self.counters[depth] = value;
    }

    pub fn add(&self, other: &Self) -> Self {
        self.combine(other, 1)
    }

    pub fn sub(&self, other: &Self) -> Self {
        self.combine(other, -1)
    }

    /// Arithmetic saturates rather than wraps: a saturated coefficient names a
    /// value no width holds, which [`AffineForm::fits`] then refuses.
    pub fn scale(&self, factor: i128) -> Self {
        Self {
            counters: self
                .counters
                .iter()
                .map(|c| c.saturating_mul(factor))
                .collect(),
            symbols: self
                .symbols
                .iter()
                .map(|c| c.saturating_mul(factor))
                .collect(),
            constant: self.constant.saturating_mul(factor),
        }
    }

    /// The scaled form, or `None` where a coefficient would leave `i128`.
    pub fn checked_scale(&self, factor: i128) -> Option<Self> {
        let scaled = self.scale(factor);
        let exact = self
            .counters
            .iter()
            .chain(&self.symbols)
            .chain([&self.constant])
            .all(|c| c.checked_mul(factor).is_some());
        exact.then_some(scaled)
    }

    /// The least and greatest value the form can name when counter `d` ranges
    /// over `counters[d]` and symbol `i` over `symbols[i]`.
    pub fn range(&self, counters: &[(i128, i128)], symbols: &[(i128, i128)]) -> (i128, i128) {
        let mut low = self.constant;
        let mut high = self.constant;
        let terms = self
            .counters
            .iter()
            .zip(counters.iter().chain(std::iter::repeat(&(0, 0))))
            .chain(
                self.symbols
                    .iter()
                    .zip(symbols.iter().chain(std::iter::repeat(&(0, 0)))),
            );
        for (&coefficient, &(min, max)) in terms {
            let (a, b) = (
                coefficient.saturating_mul(min),
                coefficient.saturating_mul(max),
            );
            low = low.saturating_add(a.min(b));
            high = high.saturating_add(a.max(b));
        }
        (low, high)
    }

    /// Whether every value in `[low, high]` is one a signed `width`-bit integer
    /// names, so arithmetic reaching it in that width did not wrap.
    pub fn fits(width: u32, low: i128, high: i128) -> bool {
        if width == 0 || width > 127 {
            return false;
        }
        let bound = 1i128 << (width - 1);
        low >= -bound && high < bound
    }

    fn combine(&self, other: &Self, sign: i128) -> Self {
        let mut result = self.clone();
        result.grow_counters(other.counters.len());
        result.grow_symbols(other.symbols.len());
        for (slot, value) in result.counters.iter_mut().zip(&other.counters) {
            *slot = slot.saturating_add(sign * value);
        }
        for (slot, value) in result.symbols.iter_mut().zip(&other.symbols) {
            *slot = slot.saturating_add(sign * value);
        }
        result.constant = result.constant.saturating_add(sign * other.constant);
        result
    }

    fn grow_counters(&mut self, len: usize) {
        if self.counters.len() < len {
            self.counters.resize(len, 0);
        }
    }

    fn grow_symbols(&mut self, len: usize) {
        if self.symbols.len() < len {
            self.symbols.resize(len, 0);
        }
    }
}
