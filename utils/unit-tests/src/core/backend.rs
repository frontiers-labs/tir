//! Assembly lexing/parsing and the machine-memory adapter.

use std::collections::HashMap;

use tir::backend::{lex, MachineContext, MachineMemory, SimTrap, Token};
use tir::sem::Memory;
use tir_adt::APInt;

#[test]
fn a_virtual_op_reports_itself_and_nothing_else() {
    // `vret` is replaced before printing, encoding or scheduling ever ask, so
    // its record carries only its identity and its control transfer.
    let context = tir::Context::with_default_dialects();
    let vret = tir::backend::VirtualReturnOpBuilder::new(&context).build();
    let info = tir::backend::MachineInstruction::info(&vret);

    assert_eq!(info.name, "vret");
    assert_eq!(info.control_flow, tir::backend::ControlFlow::Unconditional);
    assert!(info.asm.is_none());
    assert!(info.encode.is_none());
    assert!(info.sched.is_empty());
}

#[test]
fn asm_rejects_unknown_punctuation_without_panicking() {
    assert_eq!(lex(".0"), Err(()));
}

#[test]
fn asm_accepts_single_character_identifiers_and_labels() {
    assert_eq!(lex("a b:"), Ok(vec![Token::Ident("a"), Token::Label("b")]));
}

#[test]
fn asm_lexes_directives_and_string_literals() {
    assert_eq!(
        lex(".dword 0x100000000"),
        Ok(vec![
            Token::Directive(".dword"),
            Token::HexNumber("0x100000000")
        ])
    );
    assert_eq!(
        lex(".string \"Hello, RISC-V!\""),
        Ok(vec![
            Token::Directive(".string"),
            Token::StringLit("Hello, RISC-V!")
        ])
    );
    // Dedicated tokens still win over the directive catch-all.
    assert_eq!(lex(".text .data"), Ok(vec![Token::Text, Token::Data]));
}

#[test]
fn asm_lexes_memory_operand_punctuation() {
    assert_eq!(
        lex("mov rax, [rbx]"),
        Ok(vec![
            Token::Ident("mov"),
            Token::Ident("rax"),
            Token::Comma,
            Token::LBracket,
            Token::Ident("rbx"),
            Token::RBracket,
        ])
    );
    assert_eq!(
        lex("jmp *rax"),
        Ok(vec![Token::Ident("jmp"), Token::Star, Token::Ident("rax")])
    );
}

#[test]
fn asm_smoke() {
    let program = "
.text
.global _start
    _start:
    inst1 r1, r2, r3
    ret
";

    assert_eq!(
        lex(program),
        Ok(vec![
            Token::Text,
            Token::Global,
            Token::Ident("_start"),
            Token::Label("_start"),
            Token::Ident("inst1"),
            Token::Ident("r1"),
            Token::Comma,
            Token::Ident("r2"),
            Token::Comma,
            Token::Ident("r3"),
            Token::Ident("ret"),
        ])
    );
}

#[test]
fn rejects_unknown_mnemonic() {
    let context = tir::Context::with_default_dialects();
    let parser = tir::backend::AsmParser::new(HashMap::new());

    assert!(parser.parse_asm(&context, "foobar r0, r1").is_err());
}

struct StubMachine {
    memory: HashMap<u64, u8>,
    fences: u32,
}

impl StubMachine {
    fn read(&self, address: u64, size: usize) -> u64 {
        let mut word = 0u64;
        for i in 0..size {
            word |= u64::from(*self.memory.get(&(address + i as u64)).unwrap_or(&0)) << (i * 8);
        }
        word
    }
}

impl MachineContext for StubMachine {
    fn read_register(&self, _class: &str, _index: u16) -> Result<APInt, SimTrap> {
        unimplemented!()
    }
    fn write_register(&mut self, _class: &str, _index: u16, _value: APInt) -> Result<(), SimTrap> {
        unimplemented!()
    }
    fn read_memory(&self, address: u64, size: usize) -> Result<u64, SimTrap> {
        Ok(self.read(address, size))
    }
    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), SimTrap> {
        for i in 0..size {
            self.memory
                .insert(address + i as u64, (value >> (i * 8)) as u8);
        }
        Ok(())
    }
    fn fence(&mut self, _pred: u32, _succ: u32, _kind: u32) -> Result<(), SimTrap> {
        self.fences += 1;
        Ok(())
    }
    fn read_pc(&self) -> u64 {
        0
    }
    fn write_pc(&mut self, _value: u64) {}
}

#[test]
fn machine_memory_forwards_to_machine_context() {
    let mut machine = StubMachine {
        memory: HashMap::new(),
        fences: 0,
    };

    {
        let mut memory = MachineMemory(&mut machine);
        memory.write_memory(0x100, 4, 0xdeadbeef).unwrap();
        assert_eq!(memory.read_memory(0x100, 4).unwrap(), 0xdeadbeef);
        memory.fence(0b11, 0b11, 0).unwrap();
    }

    assert_eq!(machine.read(0x100, 4), 0xdeadbeef);
    assert_eq!(machine.fences, 1);
}
