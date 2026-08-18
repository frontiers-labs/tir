use tir_symbolic::btor2::Builder;

#[test]
fn emits_typed_nodes_and_reuses_sorts() {
    let mut model = Builder::new();
    let lhs = model.input(8, "lhs");
    let rhs = model.input(8, "rhs");
    let sum = model.binary("add", lhs, rhs, false);
    let matches = model.compare("eq", sum, lhs);
    model.bad(matches, "sum_matches_lhs");

    assert_eq!(
        model.to_string(),
        "1 sort bitvec 8\n\
         2 input 1 lhs\n\
         3 input 1 rhs\n\
         4 add 1 2 3\n\
         5 sort bitvec 1\n\
         6 eq 5 4 2\n\
         7 bad 6 sum_matches_lhs\n"
    );
}
