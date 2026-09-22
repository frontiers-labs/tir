use proptest::prelude::*;
use tir::passes::destructure::{ControlKind, ControlOutcome, RecoveryPlan};

use super::fixtures;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn shared_selector_requires_matching_partitions(
        first_arity in 2usize..6,
        second_arity in 3usize..6,
    ) {
        let fixture = include_str!("../../../../core/checks/Destructure/pcfr-shared-wider-selector.tir")
            .replace("func.return %result", "-> %result");
        // Each generated pair also checks an equal-arity control, so loss of
        // Direct eligibility cannot make the mismatch assertion pass vacuously.
        for first_arity in [second_arity, first_arity] {
            let first_extra = "-> %b\n      }\n      {\n        ".repeat(first_arity - 2);
            let second_extra = "-> %z\n      }\n      {\n        ".repeat(second_arity - 3);
            let source = fixture
                .replace("-> %b", &format!("{first_extra}-> %b"))
                .replace("-> %z", &format!("{second_extra}-> %z"));
            let (context, _module, _func, body) = fixtures::parse_function(&source);
            let plan = RecoveryPlan::analyze(&context, body).expect("selector analysis succeeds");
            let direct: Vec<_> = plan
                .requirements()
                .filter(|control| control.kind == ControlKind::Direct)
                .collect();
            prop_assert_eq!(direct.is_empty(), first_arity != second_arity);
            let partition: Vec<_> = (0..second_arity - 1)
                .map(|index| ControlOutcome::Exact(index as u64))
                .chain([ControlOutcome::DefaultFrom(second_arity - 1)])
                .collect();
            for control in direct {
                prop_assert_eq!(&control.outcomes, &partition);
            }
        }
    }
}
