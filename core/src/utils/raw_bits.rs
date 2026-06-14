const BYTE_SIZE: usize = 8;

#[derive(Clone, Debug)]
pub struct RawBits {
    storage: Vec<u8>,
}

impl RawBits {
    pub fn new(n: usize) -> Self {
        assert!(n % 8 == 0);
        let num_bytes = n / BYTE_SIZE;

        RawBits {
            storage: vec![0; num_bytes],
        }
    }

    pub fn width(&self) -> usize {
        self.storage.len() * BYTE_SIZE
    }

    // T is APInt or APFloat
    pub fn split<T>(self, lanes: usize) -> Vec<T> {
        todo!()
    }
}
