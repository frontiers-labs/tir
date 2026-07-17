//! Construction and text emission for BTOR2 transition systems.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitVec {
    pub nid: u32,
    pub width: u32,
    pub signed: bool,
}

pub struct Builder {
    output: String,
    next_id: u32,
    sorts: BTreeMap<u32, u32>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self {
            output: String::new(),
            next_id: 0,
            sorts: BTreeMap::new(),
        }
    }

    pub fn comment(&mut self, text: &str) {
        self.output.push_str("; ");
        self.output.push_str(text);
        self.output.push('\n');
    }

    pub fn line(&mut self, body: &str) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        self.output.push_str(&format!("{id} {body}\n"));
        id
    }

    pub fn sort(&mut self, width: u32) -> u32 {
        if let Some(sort) = self.sorts.get(&width) {
            return *sort;
        }
        let sort = self.line(&format!("sort bitvec {width}"));
        self.sorts.insert(width, sort);
        sort
    }

    pub fn input(&mut self, width: u32, name: &str) -> BitVec {
        let sort = self.sort(width);
        let nid = self.line(&format!("input {sort} {name}"));
        BitVec {
            nid,
            width,
            signed: false,
        }
    }

    pub fn constant(&mut self, width: u32, value: u64) -> BitVec {
        let sort = self.sort(width);
        let nid = self.line(&format!("constd {sort} {value}"));
        BitVec {
            nid,
            width,
            signed: false,
        }
    }

    pub fn binary(&mut self, operator: &str, lhs: BitVec, rhs: BitVec, signed: bool) -> BitVec {
        debug_assert_eq!(lhs.width, rhs.width);
        let sort = self.sort(lhs.width);
        let nid = self.line(&format!("{operator} {sort} {} {}", lhs.nid, rhs.nid));
        BitVec {
            nid,
            width: lhs.width,
            signed,
        }
    }

    pub fn compare(&mut self, operator: &str, lhs: BitVec, rhs: BitVec) -> BitVec {
        debug_assert_eq!(lhs.width, rhs.width);
        let sort = self.sort(1);
        let nid = self.line(&format!("{operator} {sort} {} {}", lhs.nid, rhs.nid));
        BitVec {
            nid,
            width: 1,
            signed: false,
        }
    }

    pub fn not(&mut self, value: BitVec) -> BitVec {
        let sort = self.sort(value.width);
        let nid = self.line(&format!("not {sort} {}", value.nid));
        BitVec { nid, ..value }
    }

    pub fn ite(
        &mut self,
        condition: BitVec,
        then_value: BitVec,
        else_value: BitVec,
        signed: bool,
    ) -> BitVec {
        debug_assert_eq!(condition.width, 1);
        debug_assert_eq!(then_value.width, else_value.width);
        let sort = self.sort(then_value.width);
        let nid = self.line(&format!(
            "ite {sort} {} {} {}",
            condition.nid, then_value.nid, else_value.nid
        ));
        BitVec {
            nid,
            width: then_value.width,
            signed,
        }
    }

    pub fn extend(&mut self, operator: &str, value: BitVec, by: u32, signed: bool) -> BitVec {
        if by == 0 {
            return BitVec { signed, ..value };
        }
        let width = value.width + by;
        let sort = self.sort(width);
        let nid = self.line(&format!("{operator} {sort} {} {by}", value.nid));
        BitVec { nid, width, signed }
    }

    pub fn slice(&mut self, value: BitVec, high: u32, low: u32) -> BitVec {
        let width = high - low + 1;
        if width == value.width {
            return value;
        }
        let sort = self.sort(width);
        let nid = self.line(&format!("slice {sort} {} {high} {low}", value.nid));
        BitVec {
            nid,
            width,
            signed: false,
        }
    }

    pub fn concat(&mut self, lhs: BitVec, rhs: BitVec) -> BitVec {
        let width = lhs.width + rhs.width;
        let sort = self.sort(width);
        let nid = self.line(&format!("concat {sort} {} {}", lhs.nid, rhs.nid));
        BitVec {
            nid,
            width,
            signed: false,
        }
    }

    pub fn widen(&mut self, value: BitVec, target: u32, signed: bool) -> BitVec {
        if value.width >= target {
            return value;
        }
        let operator = if signed { "sext" } else { "uext" };
        self.extend(operator, value, target - value.width, signed)
    }

    pub fn fit(&mut self, value: BitVec, target: u32) -> BitVec {
        if value.width > target {
            self.slice(value, target - 1, 0)
        } else {
            self.widen(value, target, value.signed)
        }
    }

    pub fn coerce(&mut self, lhs: BitVec, rhs: BitVec) -> (BitVec, BitVec) {
        let width = lhs.width.max(rhs.width);
        (
            self.widen(lhs, width, lhs.signed),
            self.widen(rhs, width, rhs.signed),
        )
    }

    pub fn as_bool(&mut self, value: BitVec) -> BitVec {
        if value.width == 1 {
            return value;
        }
        let zero = self.constant(value.width, 0);
        self.compare("neq", value, zero)
    }

    pub fn output(&mut self, value: BitVec, name: &str) {
        self.line(&format!("output {} {name}", value.nid));
    }

    pub fn bad(&mut self, condition: BitVec, name: &str) {
        self.line(&format!("bad {} {name}", condition.nid));
    }

    pub fn as_str(&self) -> &str {
        &self.output
    }
}

impl fmt::Display for Builder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.output)
    }
}
