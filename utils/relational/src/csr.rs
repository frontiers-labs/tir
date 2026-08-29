/// Compressed sparse rows: a `key -> [value]` map built by counting sort, so a
/// group costs an offset rather than a `Vec`.
#[derive(Clone, Debug, Default)]
pub struct Csr {
    offsets: Vec<u32>,
    data: Vec<u32>,
}

impl Csr {
    /// Group `entries` (`(key, value)`) under `keys` keys. Values keep the order
    /// they were given in, so a caller that hands over rows in row order reads
    /// them back in row order.
    pub fn build(keys: usize, entries: impl IntoIterator<Item = (u32, u32)> + Clone) -> Self {
        let mut offsets = vec![0u32; keys + 1];
        for (key, _) in entries.clone() {
            offsets[key as usize + 1] += 1;
        }
        for i in 0..keys {
            offsets[i + 1] += offsets[i];
        }
        let mut cursor = offsets.clone();
        let mut data = vec![0u32; offsets[keys] as usize];
        for (key, value) in entries {
            let slot = &mut cursor[key as usize];
            data[*slot as usize] = value;
            *slot += 1;
        }
        Self { offsets, data }
    }

    pub fn get(&self, key: u32) -> &[u32] {
        let key = key as usize;
        if key + 1 >= self.offsets.len() {
            return &[];
        }
        &self.data[self.offsets[key] as usize..self.offsets[key + 1] as usize]
    }

    pub fn keys(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_values_in_insertion_order() {
        let csr = Csr::build(3, vec![(2, 10), (0, 20), (2, 30)]);
        assert_eq!(csr.get(0), &[20]);
        assert_eq!(csr.get(1), &[]);
        assert_eq!(csr.get(2), &[10, 30]);
        assert_eq!(csr.get(7), &[]);
    }

    #[test]
    fn empty_build_has_no_data() {
        let csr = Csr::build(0, Vec::new());
        assert!(csr.is_empty());
        assert_eq!(csr.get(0), &[]);
    }
}
