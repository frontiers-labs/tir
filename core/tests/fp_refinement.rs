use tir::attributes::AttributeValue;
use tir::builtin::ModuleOp;
use tir::fp::ops::{AddOp, FmaOp, MulOp, NegOp, RoundOp};
use tir::fp::{IntermediateExceptions, NaNPolicy, Rounding, RoundingMode, Semantics};
use tir::sem::fp_refinement::{
    ContractionCandidate, FusedForm, RefinementError, check_contraction,
};
use tir::{Context, OpId, Operation};

const ROUND_MODULE: &str = r#"
#contract = {accuracy = {kind = "reference"}, arithmetic = {exceptions = "ignore", kind = "arithmetic", nan = "any_quiet", rounding = "nearest_even", subnormals = "gradual", tininess = "after_rounding"}, assumptions = {finite = false}, environment_epoch = 0, intermediate_exceptions = "allow_contracted_group_removal", permissions = {approved_approximation = false, cross_statement_contraction = false, expression_contraction = true, ignore_signed_zero = false, reassociation = false, reciprocal = false}, result_formats = [!f64]}
module {
  %f = func.func @contract(%a: !f64, %b: !f64, %c: !f64) -> !f64 {
    %result = fp.round (%x = %a, %y = %b, %z = %c) {contract = #contract} : !f64 {
      %product = fp.mul %x, %y : !f64
      %sum = fp.add %product, %z : !f64
      -> %sum
    }
    %candidate = fp.fma %a, %b, %c : !f64
    %independent = fp.fma %a, %b, %c : !f64
    -> %result
  }
  module_end
}
"#;

const SHARED_OUTPUT_MODULE: &str = r#"
#contract = {accuracy = {kind = "reference"}, arithmetic = {exceptions = "ignore", kind = "arithmetic", nan = "any_quiet", rounding = "nearest_even", subnormals = "gradual", tininess = "after_rounding"}, assumptions = {finite = false}, environment_epoch = 0, intermediate_exceptions = "allow_contracted_group_removal", permissions = {approved_approximation = false, cross_statement_contraction = false, expression_contraction = true, ignore_signed_zero = false, reassociation = false, reciprocal = false}, result_formats = [!f64, !f64]}
module {
  %f = func.func @contract(%a: !f64, %b: !f64, %c: !f64) -> !f64 {
    %first, %second = fp.round (%x = %a, %y = %b, %z = %c) {contract = #contract} : (!f64, !f64) {
      %product = fp.mul %x, %y : !f64
      %sum = fp.add %product, %z : !f64
      %left = fp.neg %sum : !f64
      %right = fp.neg %sum : !f64
      -> %left, %right
    }
    %candidate = fp.fma %a, %b, %c : !f64
    %candidate_left = fp.neg %candidate : !f64
    %candidate_right = fp.neg %candidate : !f64
    %independent = fp.fma %a, %b, %c : !f64
    %independent_right = fp.neg %independent : !f64
    -> %first
  }
  module_end
}
"#;

const DOUBLE_PRODUCT_MODULE: &str = r#"
#contract = {accuracy = {kind = "reference"}, arithmetic = {exceptions = "ignore", kind = "arithmetic", nan = "any_quiet", rounding = "nearest_even", subnormals = "gradual", tininess = "after_rounding"}, assumptions = {finite = false}, environment_epoch = 0, intermediate_exceptions = "allow_contracted_group_removal", permissions = {approved_approximation = false, cross_statement_contraction = false, expression_contraction = true, ignore_signed_zero = false, reassociation = false, reciprocal = false}, result_formats = [!f64]}
module {
  %f = func.func @contract(%a: !f64, %b: !f64) -> !f64 {
    %result = fp.round (%x = %a, %y = %b) {contract = #contract} : !f64 {
      %product = fp.mul %x, %y : !f64
      %sum = fp.add %product, %product : !f64
      -> %sum
    }
    %retained = fp.mul %a, %b : !f64
    %candidate = fp.fma %a, %b, %retained : !f64
    -> %result
  }
  module_end
}
"#;

#[test]
fn changed_nan_policy_invalidates_witness() {
    let fixture = Fixture::new();
    let witness = check_contraction(&fixture.context, &fixture.round, &fixture.candidate).unwrap();

    let mut arithmetic = fixture.round.contract().arithmetic;
    arithmetic.nan = NaNPolicy::Canonical;
    fixture.set_arithmetic(arithmetic);

    let current = check_contraction(&fixture.context, &fixture.round, &fixture.candidate).unwrap();
    assert_ne!(witness.fingerprint(), current.fingerprint());
    assert_ne!(witness, current);
    assert_eq!(
        witness.validate(&fixture.context, &fixture.round, &fixture.candidate),
        Err(RefinementError::StaleOperation)
    );
}

#[test]
fn changed_rounding_mode_invalidates_witness() {
    let fixture = Fixture::new();
    let witness = check_contraction(&fixture.context, &fixture.round, &fixture.candidate).unwrap();

    let mut arithmetic = fixture.round.contract().arithmetic;
    arithmetic.rounding = Rounding::Fixed(RoundingMode::TowardZero);
    fixture.set_arithmetic(arithmetic);

    let current = check_contraction(&fixture.context, &fixture.round, &fixture.candidate).unwrap();
    assert_ne!(witness.fingerprint(), current.fingerprint());
    assert_ne!(witness, current);
    assert_eq!(
        witness.validate(&fixture.context, &fixture.round, &fixture.candidate),
        Err(RefinementError::StaleOperation)
    );
}

#[test]
fn changed_effect_requirement_invalidates_witness() {
    let fixture = Fixture::new();
    let witness = check_contraction(&fixture.context, &fixture.round, &fixture.candidate).unwrap();

    let mut contract = (*fixture.round.contract()).clone();
    contract.intermediate_exceptions = IntermediateExceptions::Preserve;
    fixture.set_attr(
        fixture.round.id(),
        "contract",
        AttributeValue::EvaluationContract(fixture.context.intern_evaluation_contract(contract)),
    );

    let current = check_contraction(&fixture.context, &fixture.round, &fixture.candidate).unwrap();
    assert_ne!(witness.fingerprint(), current.fingerprint());
    assert_ne!(witness, current);
    assert_eq!(
        witness.validate(&fixture.context, &fixture.round, &fixture.candidate),
        Err(RefinementError::StaleOperation)
    );
}

#[test]
fn independent_candidate_output_cannot_reuse_witness() {
    let context = Context::with_default_dialects();
    let module = tir::parse::ir::parse_ir::<ModuleOp>(&context, SHARED_OUTPUT_MODULE).unwrap();
    let root = module.id();
    tir::verify_op_tree(&context, root).unwrap();
    let round = context
        .get_op(find_ops::<RoundOp>(&context, root)[0])
        .as_op::<RoundOp>()
        .unwrap();
    let negs = find_ops::<NegOp>(&context, root);
    let fmas = find_ops::<FmaOp>(&context, root);
    let candidate = ContractionCandidate {
        mul: find_ops::<MulOp>(&context, root)[0],
        add_or_sub: find_ops::<AddOp>(&context, root)[0],
        implementation: fmas[0],
        fused_operands: [
            round.reference_region().ports()[0].id(),
            round.reference_region().ports()[1].id(),
            round.reference_region().ports()[2].id(),
        ],
        form: FusedForm::MulAdd,
        result_mapping: round
            .reference_region()
            .results()
            .iter()
            .zip([negs[2], negs[3]])
            .map(|(source, output)| (*source, context.get_op(output).value_results()[0]))
            .collect(),
    };
    let witness = check_contraction(&context, &round, &candidate).unwrap();
    let mut changed = candidate.clone();
    changed.result_mapping[1].1 = context.get_op(negs[4]).value_results()[0];

    assert!(check_contraction(&context, &round, &changed).is_err());
    assert!(witness.validate(&context, &round, &changed).is_err());
}

#[test]
fn double_product_contraction_retains_product_as_addend() {
    let context = Context::with_default_dialects();
    let module = tir::parse::ir::parse_ir::<ModuleOp>(&context, DOUBLE_PRODUCT_MODULE).unwrap();
    let root = module.id();
    tir::verify_op_tree(&context, root).unwrap();
    let round = context
        .get_op(find_ops::<RoundOp>(&context, root)[0])
        .as_op::<RoundOp>()
        .unwrap();
    let products = find_ops::<MulOp>(&context, root);
    let add = find_ops::<AddOp>(&context, root)[0];
    let fma = find_ops::<FmaOp>(&context, root)[0];
    let candidate = ContractionCandidate {
        mul: products[0],
        add_or_sub: add,
        implementation: fma,
        fused_operands: [
            round.reference_region().ports()[0].id(),
            round.reference_region().ports()[1].id(),
            context.get_op(products[0]).value_results()[0],
        ],
        form: FusedForm::MulAdd,
        result_mapping: vec![(
            round.reference_region().results()[0],
            context.get_op(fma).value_results()[0],
        )],
    };

    check_contraction(&context, &round, &candidate).unwrap();
}

struct Fixture {
    context: Context,
    round: RoundOp,
    candidate: ContractionCandidate,
}

impl Fixture {
    fn new() -> Self {
        let context = Context::with_default_dialects();
        let module = tir::parse::ir::parse_ir::<ModuleOp>(&context, ROUND_MODULE).unwrap();
        let root = module.id();
        let round = context
            .get_op(find_ops::<RoundOp>(&context, root)[0])
            .as_op::<RoundOp>()
            .unwrap();
        let mul = find_ops::<MulOp>(&context, root)[0];
        let add = find_ops::<AddOp>(&context, root)[0];
        let fma = find_ops::<FmaOp>(&context, root)[0];
        let candidate = ContractionCandidate {
            mul,
            add_or_sub: add,
            implementation: fma,
            fused_operands: [
                round.reference_region().ports()[0].id(),
                round.reference_region().ports()[1].id(),
                round.reference_region().ports()[2].id(),
            ],
            form: FusedForm::MulAdd,
            result_mapping: vec![(
                round.reference_region().results()[0],
                context.get_op(fma).value_results()[0],
            )],
        };
        Self {
            context,
            round,
            candidate,
        }
    }

    fn set_arithmetic(&self, arithmetic: tir::fp::ArithmeticSemantics) {
        let mut contract = (*self.round.contract()).clone();
        contract.arithmetic = arithmetic;
        self.set_attr(
            self.round.id(),
            "contract",
            AttributeValue::EvaluationContract(self.context.intern_evaluation_contract(contract)),
        );
        for operation in [
            self.candidate.mul,
            self.candidate.add_or_sub,
            self.candidate.implementation,
        ] {
            self.set_attr(
                operation,
                "semantics",
                AttributeValue::FpSemantics(
                    self.context
                        .intern_fp_semantics(Semantics::Arithmetic(arithmetic)),
                ),
            );
        }
    }

    fn set_attr(&self, operation: OpId, name: &str, value: AttributeValue) {
        let mut attributes = self.context.get_op(operation).attributes();
        let attribute = attributes
            .iter_mut()
            .find(|attribute| self.context.resolve(attribute.name) == name)
            .unwrap();
        attribute.value = value;
        self.context.set_op_attributes(operation, attributes);
    }
}

fn find_ops<T: Operation>(context: &Context, root: OpId) -> Vec<OpId> {
    let operation = context.get_op(root);
    let mut found = operation
        .is::<T>()
        .then_some(root)
        .into_iter()
        .collect::<Vec<_>>();
    for child in operation
        .regions()
        .iter()
        .flat_map(|region| context.get_region(*region).op_ids())
    {
        found.extend(find_ops::<T>(context, child));
    }
    found
}
