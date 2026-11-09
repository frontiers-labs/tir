#![no_main]

use libfuzzer_sys::fuzz_target;
use tir_cdsl::{compile, SourceMap, Value};

struct Program<'a> {
    bytes: std::slice::Iter<'a, u8>,
}

impl Program<'_> {
    fn byte(&mut self) -> u8 {
        self.bytes.next().copied().unwrap_or(0)
    }

    fn expression(&mut self, depth: usize) -> (String, i128) {
        let choice = self.byte();
        if depth == 0 {
            let value = i128::from(choice % 17);
            return (value.to_string(), value);
        }
        let (left, a) = self.expression(depth - 1);
        match choice % 11 {
            0 => (format!("id<Integer>({left})"), a),
            1 => (format!("value<Integer>({{extra:false,value:{left}}})"), a),
            2 => (format!("make(7)({left})"), a + 7),
            3 => (
                format!("match Choice::Pick({left}){{Choice::Pick(x)=>x,Choice::Empty()=>99}}"),
                a,
            ),
            4 => (
                format!("example::add<Integer>.payload.exec({left},3)"),
                a + 3,
            ),
            5 => (
                format!("example::constant<Integer>.payload.exec({{value:{left}}})"),
                a,
            ),
            6 => {
                let n = self.byte() % 9;
                let mut chain = "Chain::Nil<Integer>()".to_owned();
                for _ in 0..n {
                    chain = format!("Chain::Cons<Integer>(1,{chain})");
                }
                (
                    format!("({left}+length<Integer>({chain}))"),
                    a + i128::from(n),
                )
            }
            7 => (format!("({left}+library::answer)"), a + 2),
            operator => {
                let (right, b) = self.expression(depth - 1);
                match operator {
                    8 => (format!("({left}+{right})"), a + b),
                    9 => (format!("({left}-{right})"), a - b),
                    _ => (
                        format!("if {left}<{right}{{{left}}}else{{{right}}}"),
                        a.min(b),
                    ),
                }
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 {
        return;
    }
    let (expression, expected) = Program { bytes: data.iter() }.expression(4);
    let source = format!(
        "mod library;let id<T>=fn(x:T)->T{{x}};let value<T>=fn(record)->T{{record.value}};\
         let make=fn(x:Integer){{fn(y:Integer){{x+y}}}};enum Choice{{Empty,Pick(Integer)}};\
         enum Chain<T>{{Nil,Cons(T,Chain<T>)}};\
         let rec length<T>=fn(xs:Chain<T>)->Integer{{match xs{{Chain::Nil()=>0,Chain::Cons(h,t)=>1+length<T>(t)}}}};\
         extern type Namespace=Type;extern type example=Namespace;\
         pub let example::add<T>={{signature:(lhs:T,rhs:T)->T,exec:fn(lhs:T,rhs:T)->T{{lhs+rhs}}}};\
         pub let example::constant<T>={{signature:()->T,attributes:{{value:T}},exec:fn(self)->T{{self.value}}}};\
         let main={expression};"
    );
    let mut sources = SourceMap::new("oracle.cdsl", source);
    sources.insert("library", "pub let answer=2;");
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}\n{}", sources.root));
    let actual = checked
        .evaluate("main")
        .unwrap_or_else(|error| panic!("{error}\n{}", sources.root));
    assert!(
        matches!(actual, Value::Integer(n) if n == expected),
        "expected {expected}, got {actual}\n{}",
        sources.root
    );
});
