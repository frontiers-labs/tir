use tir::backend::binary::{BinaryWriter, ObjectFile, ObjectFormatInfo};
use tir::backend::{MachineBlockLayoutPass, SectionOp, SymbolOp};
use tir::builtin::ModuleOp;
use tir::{AnalysisManager, BlockId, Context, Operation, OperationRef, Pass};

struct Assembly {
    context: Context,
    module: ModuleOp,
    symbol: SymbolOp,
    format: ObjectFormatInfo,
}

impl Assembly {
    fn new(source: &str) -> Self {
        let context = Context::with_default_dialects();
        let target = tir::backend::select_target("x86_64", None, None).unwrap();
        target.register_dialects(&context);
        let module = target
            .asm_parser(&context)
            .parse_asm(&context, source)
            .unwrap();
        let section = context
            .get_op(module.body().op_ids()[0])
            .as_op::<SectionOp>()
            .unwrap();
        let symbol = context
            .get_op(section.body().op_ids()[0])
            .as_op::<SymbolOp>()
            .unwrap();
        Self {
            context,
            module,
            symbol,
            format: target.object_format().unwrap(),
        }
    }

    fn layout(&self) {
        MachineBlockLayoutPass::new(self.format)
            .run(
                &OperationRef::new(self.context.get_op(self.symbol.id())),
                &self.context,
                &AnalysisManager::new(),
            )
            .unwrap();
    }

    fn order(&self) -> Vec<BlockId> {
        tir::backend::symbol_body_blocks(&self.context, &self.context.get_op(self.symbol.id()))
    }

    fn object(&self) -> ObjectFile {
        BinaryWriter::new()
            .write_module(&self.context, &self.module, &self.format)
            .unwrap()
    }
}

fn chain(prefix: &str, order: &[usize]) -> String {
    let mut source = format!(".global chain\nchain:\n{prefix}");
    for index in order {
        source.push_str(&format!("block{index}:\n jmp block{}\n", index + 1));
    }
    source.push_str(&format!("block{}:\n ret\n", order.len()));
    source
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 32,
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn a_permuted_straight_line_path_emits_no_jumps(keys in proptest::collection::vec(proptest::num::u32::ANY, 2..16)) {
        let mut order: Vec<_> = (0..keys.len()).collect();
        order.sort_by_key(|&index| keys[index]);
        let assembly = Assembly::new(&chain(" jmp block0\n", &order));
        let original = assembly.symbol.body().op_ids();
        assembly.layout();
        proptest::prop_assert_eq!(assembly.symbol.body().op_ids(), original);
        let object = assembly.object();
        proptest::prop_assert_eq!(object.sections[0].data.as_slice(), &[0xc3]);
    }

    #[test]
    fn an_indirect_exit_keeps_the_original_layout(block_count in 2usize..16) {
        let order: Vec<_> = (0..block_count).rev().collect();
        let assembly = Assembly::new(&chain(
            " test rdi, rdi\n jne indirect\n jmp block0\nindirect:\n jmp *rax\n", &order,
        ));
        let original = assembly.order();
        assembly.layout();
        proptest::prop_assert_eq!(assembly.order(), original);
    }

    #[test]
    fn a_raw_relative_transfer_keeps_the_original_layout(block_count in 2usize..16, annotated in proptest::bool::ANY) {
        let order: Vec<_> = (0..block_count).rev().collect();
        let assembly = Assembly::new(&chain(" jrcxz 5\n jmp block0\n", &order));
        let original = assembly.order();
        if annotated {
            let branch = assembly.context.get_op(assembly.symbol.body().op_ids()[0]);
            let mut attributes = branch.attributes();
            attributes.push(assembly.context.named_attribute(
                "note", tir::attributes::AttributeValue::Str("diagnostic".into()),
            ));
            assembly.context.set_op_attributes(branch.id, attributes);
        }
        assembly.layout();
        proptest::prop_assert_eq!(assembly.order(), original);
    }

    #[test]
    fn a_symbolic_call_does_not_prevent_layout(block_count in 2usize..16) {
        let order: Vec<_> = (0..block_count).rev().collect();
        let assembly = Assembly::new(&chain(" call external\n jmp block0\n", &order));
        assembly.layout();
        let object = assembly.object();
        let text = &object.sections[0];
        proptest::prop_assert_eq!(text.data.as_slice(), &[0xe8, 0, 0, 0, 0, 0xc3]);
        proptest::prop_assert_eq!(text.relocs.len(), 1);
        proptest::prop_assert_eq!(&text.relocs[0].symbol, "external");
    }

    #[test]
    fn a_short_branch_limits_block_motion(padding_count in 16usize..24) {
        let mut source = String::from(".global bounded\nbounded:\n jrcxz near\n jmp padding\nnear:\n jmp done\npadding:\n");
        for _ in 0..padding_count {
            source.push_str(" mov rbx, 0\n");
        }
        source.push_str(" jmp near\ndone:\n ret\n");
        let assembly = Assembly::new(&source);
        let original = assembly.order();
        let before = assembly.object();
        assembly.layout();
        let after = assembly.object();
        if 7 * padding_count <= 127 {
            proptest::prop_assert_ne!(assembly.order(), original);
            proptest::prop_assert_eq!(after.sections[0].data.len() + 15, before.sections[0].data.len());
        } else {
            proptest::prop_assert_eq!(assembly.order(), original);
            proptest::prop_assert_eq!(after, before);
        }
    }
}
