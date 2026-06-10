//! Program image construction: lowering a parsed assembly module into a flat,
//! addressed sequence of basic blocks the executor can fetch from.
//!
//! The block — not the instruction — is the unit of fetch. Control transfers
//! always land on a block start, which is what lets the executor run a whole
//! block straight-line and lets a timing model (or, later, a JIT) treat blocks
//! as units of translation.

use std::collections::{BTreeMap, HashMap};

use tir::attributes::AttributeValue;
use tir::builtin::ModuleOp;
use tir::{BlockId, Context, OpId, Operation};
use tir_be_common::{MachineInstruction, SectionOp, SymbolOp};

use crate::error::Error;

/// One basic block of the loaded program, addressed and sized.
#[derive(Debug, Clone)]
pub struct MachineBlock {
    /// The IR block backing this machine block, for tools that need to walk
    /// back to the IR (e.g. a future block-level JIT).
    pub block: BlockId,
    pub instructions: Vec<OpId>,
    pub start_address: u64,
    pub byte_len: u64,
    pub fallthrough_pc: Option<u64>,
}

/// An executable program: addressed blocks plus the IR context that owns the
/// instruction operations they reference.
#[derive(Clone)]
pub struct ProgramImage {
    pub context: Context,
    pub entry_pc: u64,
    pub symbols: BTreeMap<String, u64>,
    pub blocks: Vec<MachineBlock>,
    pub block_map_by_start: HashMap<u64, usize>,
}

impl ProgramImage {
    /// Lay out every symbol of `module` at consecutive addresses starting at
    /// `base_address` and resolve the entry point. Each symbol becomes one
    /// [`MachineBlock`]; empty symbols (pure labels) still occupy 4 bytes so
    /// every block has a distinct address.
    pub fn from_module(
        context: &Context,
        module: ModuleOp,
        base_address: u64,
        entry_symbol: Option<&str>,
    ) -> Result<Self, Error> {
        let mut symbols = BTreeMap::new();
        let mut blocks = Vec::new();
        let mut cur_pc = base_address;
        let mut first_symbol_pc = None;

        let mut blocks_to_visit = vec![module.body()];
        while let Some(block) = blocks_to_visit.pop() {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);

                if let Some(section) = op.clone().as_op::<SectionOp>() {
                    blocks_to_visit.push(section.body());
                    continue;
                }

                let Some(symbol) = op.as_op::<SymbolOp>() else {
                    continue;
                };

                let symbol_name = symbol
                    .attributes()
                    .iter()
                    .find_map(|attr| match (&*attr.name, &attr.value) {
                        ("name", AttributeValue::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .ok_or(Error::MissingSymbolName)?;
                symbols.insert(symbol_name, cur_pc);
                first_symbol_pc.get_or_insert(cur_pc);

                let symbol_block = symbol.body();
                let mut instructions = Vec::new();
                let mut byte_len = 0u64;
                for inner_id in symbol_block.op_ids() {
                    let inner_op = context.get_op(inner_id);
                    if let Some(mi) = inner_op.as_interface::<dyn MachineInstruction>() {
                        instructions.push(inner_id);
                        byte_len += u64::from(mi.width_bytes());
                    }
                }

                blocks.push(MachineBlock {
                    block: symbol_block.id(),
                    instructions,
                    start_address: cur_pc,
                    byte_len,
                    fallthrough_pc: None,
                });
                cur_pc += byte_len.max(4);
            }
        }

        if blocks.is_empty() {
            return Err(Error::NoSymbolsFound);
        }

        for i in 0..blocks.len() - 1 {
            blocks[i].fallthrough_pc = Some(blocks[i + 1].start_address);
        }

        let entry_pc = match entry_symbol {
            Some(name) => *symbols
                .get(name)
                .ok_or_else(|| Error::EntrySymbolNotFound(name.to_string()))?,
            None => first_symbol_pc.ok_or(Error::NoSymbolsFound)?,
        };

        let block_map_by_start = blocks
            .iter()
            .enumerate()
            .map(|(idx, block)| (block.start_address, idx))
            .collect();

        Ok(ProgramImage {
            context: context.clone(),
            entry_pc,
            symbols,
            blocks,
            block_map_by_start,
        })
    }

    /// The block starting exactly at `pc`, if any. Control transfers always
    /// target block starts, so this is the executor's fetch lookup.
    pub fn block_at(&self, pc: u64) -> Option<&MachineBlock> {
        self.block_map_by_start.get(&pc).map(|&i| &self.blocks[i])
    }
}
