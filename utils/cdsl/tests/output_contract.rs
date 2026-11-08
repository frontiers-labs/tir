#![cfg(feature = "rust-output")]

use tir_cdsl::output::check_operation;
use tir_cdsl::{SourceMap, compile};

#[test]
fn operation_export_checks_relationships_between_individually_typed_fields() {
    let prefix = "extern type Dialect=Type;extern type builtin=Dialect;";
    let cases = [
        (
            "{signature:(lhs:Integer)->Integer,exec:fn(lhs:Integer)->Bool{true}}",
            "exec has type",
        ),
        (
            "{signature:(lhs:Integer)->Integer,exec:fn()->Integer{0}}",
            "exec has type",
        ),
        (
            "{signature:(lhs:Integer)->Integer,attributes:{value:Bool},exec:fn(self:Record<{value:Integer}>,lhs:Integer)->Integer{lhs}}",
            "exec has type",
        ),
        (
            "{signature:()->Integer,attributes:{value:{ty:Integer,default:false}}}",
            "default has type",
        ),
        (
            "{signature:(lhs:Integer)->Integer,poison:fn(lhs:Integer)->Integer{lhs}}",
            "poison has type",
        ),
        (
            "{signature:(lhs:Integer)->Integer,requires:fn()->Bool{true}}",
            "requires has type",
        ),
        ("{signature:Integer}", "signature must describe a function"),
        (
            "{signature:()->(),exex:fn(){()}}",
            "unknown operation field",
        ),
        (
            "{signature:()->(),attributes:{flag:{ty:Bool,defualt:false}}}",
            "descriptor field",
        ),
        (
            "{signature:()->(),attributes:{callback:{ty:(x:Integer)->Integer,default:fn(x:Integer)->Integer{x}}}}",
            "requires an Attribute",
        ),
    ];
    for (record, expected) in cases {
        let source = format!("{prefix}pub let builtin::op={record};");
        let module = compile(&SourceMap::new("contract.cdsl", source))
            .unwrap_or_else(|errors| panic!("record itself must typecheck: {errors:?}"));
        let error = match check_operation(&module, "builtin::op") {
            Ok(_) => panic!("accepted invalid operation contract: {record}"),
            Err(error) => error,
        };
        assert!(error.message.contains(expected), "{error}");
    }
    let source = format!(
        "{prefix}pub let builtin::op<T>={{signature:(lhs:T)->T,attributes:{{flag:Bool}},exec:fn(self:Record<{{flag:Bool}}>,lhs:T)->T{{lhs}},poison:fn(self:Record<{{flag:Bool}}>,lhs:T)->Bool{{self.flag}}}};"
    );
    let module = compile(&SourceMap::new("contract.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let operation =
        check_operation(&module, "builtin::op").unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(operation.operands.len(), 1);
    assert!(operation.attributes.contains_key("flag"));
    let source = format!(
        "{prefix}mod ops {{pub extern type Operation=Type;pub extern type Context=Type;pub extern type Validation=Type;pub enum RegionKind{{Blocks,Nodes,Any}};}};extern let check:(op:ops::Operation,ctx:ops::Context)->ops::Validation;pub let builtin::op={{signature:()->(),verify:check,regions:{{body:{{kind:ops::RegionKind::Blocks()}}}}}};"
    );
    let module = compile(&SourceMap::new("contract.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    check_operation(&module, "builtin::op").unwrap_or_else(|error| panic!("{error}"));
}
