use std::collections::HashMap;
use std::fmt::Write;

use crate::BlockId;

pub struct IRFormatter<'a> {
    w: &'a mut dyn Write,
    padding: u8,
    new_line: bool,
    region_block_numbers: Vec<HashMap<BlockId, u32>>,
}

impl<'a> IRFormatter<'a> {
    pub fn new(w: &'a mut dyn Write) -> Self {
        Self {
            w,
            padding: 0,
            new_line: true,
            region_block_numbers: vec![],
        }
    }

    pub fn push_region_block_numbers(&mut self, numbers: HashMap<BlockId, u32>) {
        self.region_block_numbers.push(numbers);
    }

    pub fn pop_region_block_numbers(&mut self) {
        self.region_block_numbers.pop();
    }

    pub fn region_block_number(&self, block: BlockId) -> u32 {
        self.region_block_numbers
            .last()
            .and_then(|numbers| numbers.get(&block).copied())
            .unwrap_or_else(|| block.number())
    }

    pub fn push(&mut self) {
        self.padding += 1;
    }

    pub fn pop(&mut self) {
        assert_ne!(self.padding, 0);
        self.padding -= 1;
    }

    pub fn writeln<S: AsRef<str>>(&mut self, s: S) -> Result<(), std::fmt::Error> {
        if self.new_line {
            for _ in 0..self.padding {
                self.w.write_str("  ")?;
            }
        }

        self.w.write_str(s.as_ref())?;
        self.new_line = true;
        self.w.write_char('\n')
    }

    pub fn write<S: AsRef<str>>(&mut self, s: S) -> Result<(), std::fmt::Error> {
        if self.new_line {
            for _ in 0..self.padding {
                self.w.write_str("  ")?;
            }
        }

        self.new_line = s.as_ref().ends_with("\n");
        self.w.write_str(s.as_ref())
    }
}
