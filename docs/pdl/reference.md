# PDL language reference

PDL describes patterns over compiler expressions and the transformations those
patterns permit. A rule names a pattern, binds parts of a matching expression,
and describes a replacement using those bindings. The [PDL overview](index.md)
introduces the purpose of the language. [Instcombine](../design/instcombine.md)
explains one engine that executes its rules.

The PDL frontend checks syntax, names, and structural constraints. Consumers
then lower the checked rules into executable rewrites or proof obligations.
Consumers support different subsets of the language. In particular, acceptance
by the frontend does not imply support by instcombine, instruction selection,
or the prover. The [consumer support section](#consumer-support) records these
boundaries.

## Files and lexical rules

A file contains zero or more `group`, `rule`, and `refinement` declarations.
Each declaration ends with a semicolon. There are no import declarations,
user-defined functions, or nested declaration scopes.

Whitespace and line breaks separate tokens but do not otherwise affect meaning.
`//` starts a comment that extends to the end of the line. Block comments are
not supported.

Identifiers are case-sensitive. They start with an ASCII letter or underscore
and continue with letters, digits, or underscores. Rule names also allow
hyphen-separated parts, as in `mul-pow2-to-shl`. Ordinary binders do not allow
hyphens. A binder named `_` is an ordinary name; the special width wildcard is
the `_` in `int<_>`.

The reserved words are `group`, `rule`, `refinement`, `where`, `requires`,
`proof`, `phase`, `root`, `keep`, `const`, `int`, `float`, and `shaped_float`.

Integer literals use decimal, hexadecimal with `0x`, or binary with `0b`.
`0X` and `0B` prefixes are also accepted. Digits cannot contain separators.
The literal token must fit in a nonnegative signed 64-bit integer. Negative
expressions use unary `-`; there are no floating-point numeric literals.
Large bit patterns can be expressed through width-limited constants, such as
`const<64>(-1)`.

Strings use double quotes. The lexer accepts backslash escape spellings but
preserves their characters rather than decoding them. String interpretation
belongs to the consumer.

## Equality declarations

The general form is:

```text
rule NAME: LEFT => RIGHT
    where CONDITION, CONDITION
    proof MODE
    phase post-saturation;
```

The `where`, `proof`, and `phase` clauses are optional. When present, they occur
in that order. A `where` clause contains one or more comma-separated conditions,
all of which must hold.

```pdl
rule mul-pow2-to-shl:
	builtin.muli(x: int<W>, c: const) => builtin.shli(x, const<W>(ctz(c)))
	where popcount(c) == 1, c != 1;
```

`=>` gives the direction in which the engine searches and introduces an
alternative. The declaration still asserts equality under its conditions.
In an e-graph, the result joins the matched expression's equivalence class;
the original expression remains available.

`<=>` declares a bidirectional equality. The frontend accepts it, but the
instcombine Rust generator does not emit it. The semantic axiom loader uses
the written left-to-right orientation. Reverse search is not automatically
available merely because a rule parses with `<=>`.

Rule names must be unique within a compiled input. Names identify diagnostics
and rules; declaration order does not specify a sequence of destructive edits.

## Terms and operation patterns

A term describes an expression, a bound value, or a constant. Operation terms
have this shape:

```text
OPERATOR<ATTRIBUTES>(VALUE_OPERANDS | DEPENDENCY_OPERANDS) : RESULT_TYPE
```

Attributes, dependency operands, and the result type are optional. Parentheses
are required even for an operation with no operands. Value operands are
comma-separated and may have a trailing comma. Dependency operands follow `|`
and require at least one term.

There are two operation vocabularies:

| Form | Meaning | Example |
| --- | --- | --- |
| `dialect.operation(...)` | A concrete IR operation identity. | `builtin.addi(x, y)` |
| `#operator(...)` | An operation in TIR's shared semantic vocabulary. | `#add(x, y)` |

These forms are distinct. `#add` does not mean a textual abbreviation for
`builtin.addi`. A consumer relates concrete operations to semantic expressions
through their declared behavior.

Nested terms describe nested expression structure. A name can refer to the
same value at several positions:

```pdl
rule cancel-add: builtin.subi(builtin.addi(x: int<W>, y), y) => x;
```

The two occurrences of `y` must match the same equivalence class. They do not
need to be the same printed expression or the same original IR operation.

The left-hand side must be an operation or a bare constant binder such as
`v: const<W>`. A bare value binder such as `x` cannot be a whole left-hand side.

### Attributes

Attributes precede the operand list:

```text
builtin.cmpi<predicate = "eq">(x, y)
example.op<mode = 1, label = "fast", tag = $t>(x)
```

Each attribute has the form `name = value`. Values are integer literals,
strings, or attribute binders prefixed with `$`. Attributes are comma-separated
and may have a trailing comma. A left-hand `$t` binds an attribute; a right-hand
`$t` refers to that binding. The Rust generator does not currently lower
attribute patterns or attribute emission.

### Dependencies and structured control

The `|` separator distinguishes effect dependencies from ordinary value
operands. For example, the term `ptr.load(address | state)` describes a load
that depends on a particular memory state. The dialect defines the exact
operand meanings. Omitting a dependency is not a statement that the effect
can be ignored.

Semantic terms do not accept dependency operands after `|`. TIR's semantic
graph also represents memory state, but not every node in that graph has a
public PDL operator spelling.

The parser also accepts a semicolon after the first value operand. This makes
switch patterns easier to read:

```pdl
rule switch-same:
	#switch(p: int<8>; x: int<W>, x, x) : int<W> => x;
```

The semicolon separates the predicate from the arms for readability; the term
still has one ordered list of operands.

`#if(condition, then_value, else_value)` describes a conditional value.
`#theta(initial, backedge)` describes a carried value. The more explicit
`#loop(initial, backedge, published, continue_condition)` also describes the
value published on exit. `#port(label)` refers to the carried value inside
its enclosing `#loop`.

A `#port` outside a `#loop` is invalid. A right-hand loop term cannot contain
another `#theta` or `#loop` beneath it. This restriction prevents rules from
repeatedly growing a loop inside itself. Loop semantics and proof support are
separate from the syntax checks. The scalar loop prover also requires each
`#loop` to occupy the whole side on which it appears, and rejects rules that
mix `#theta` with `#loop`.

### Semantic operator families

The frontend recognizes semantic operator names and checks their operand counts.
Concrete dialect operation names and operand meanings require consumer support.
Common semantic forms include:

| Family | Operators | Meaning |
| --- | --- | --- |
| Integer arithmetic | `#add`, `#sub`, `#mul`, `#div`, `#udiv`, `#srem`, `#urem` | Binary arithmetic; `#div` and `#srem` use signed interpretation. |
| Unary integer operations | `#neg`, `#not` | Arithmetic negation and bitwise complement. |
| Bit operations | `#and`, `#or`, `#xor`, `#xnor` | Binary bitwise operations. |
| Shifts | `#shl`, `#ashr`, `#lshr` | Left, arithmetic right, and logical right shift. |
| Comparisons | `#eq`, `#ne`, `#lt`, `#le`, `#gt`, `#ge`, `#ult`, `#ule`, `#ugt`, `#uge` | Binary comparisons with a one-bit result; the `u` prefix selects unsigned ordering. |
| Width changes | `#sext`, `#zext` | Value and destination width. |
| Bit concatenation | `#concat` | Two bit vectors joined into one. |
| Conditional values | `#if`, `#switch` | A predicate followed by alternatives. |
| Carried values | `#theta`, `#loop`, `#port` | Loop values and references to them. |

Floating-point operators such as `#fadd`, `#fmul`, and `#fadd_round` also belong
to the PDL semantic vocabulary. Their formats and rounding behavior are part
of their meaning; an arithmetic resemblance alone does not establish equality.
This table lists common forms rather than freezing the entire operation vocabulary.

## Binders and types

The first occurrence of a value name on the left may declare its type.
Later occurrences use the name without a type annotation. Right-hand binders
refer to left-hand bindings and cannot introduce type annotations.

```pdl
rule sub-self: builtin.subi(x: int<W>, x) => const<W>(0);
```

Here `x` binds an integer-valued equivalence class and `W` binds its bit width.
`const<W>(0)` creates a zero of that width. A width name is compile-time
information about the match, not a runtime input to the program.

The language accepts the following type forms:

| Form | Meaning |
| --- | --- |
| `int<32>` or `i32` | An integer with a concrete bit width. |
| `int<W>` | An integer whose width binds or agrees with `W`. |
| `int<_>` | An integer with an unspecified width. |
| `float<32>`, `float<64>` | IEEE binary32 or binary64 floating-point format. |
| `shaped_float<32, 4>` | A shaped floating-point type; the current syntax carries one literal extent. |
| `state<memory>` | A memory state dependency. |
| `state<fp.env>` | A floating-point environment state dependency. |
| `state<name>` or `state<name.field>` | A named resource, subject to consumer support. |
| A named type | A consumer-defined name or a declared type group. |

`int<...>` accepts a literal width, a name, or `_`, not a width expression such
as `W + 1`. Floating-point formats are literal `32` or `64`; they cannot bind
a format variable. Recognizing a type spelling does not guarantee that a
consumer can represent or match it.

An operation's result type follows its closing parenthesis:

```pdl
rule semantic-add-zero: #add(x: int<W>, 0) : int<W> => x;
```

The result annotation constrains the operation result, while `x: int<W>`
constrains the operand. The Rust generator currently accepts named integer
width binders, but not operation result type annotations. For a concrete width
in an instcombine rule, a named width with `where W == 32` avoids the current
lowering limitation on `int<32>` binders.

### Constant binders

`c: const` binds a class with a known integer constant. `c: const<W>` also
binds or constrains its width. `c: const<32>` requires a 32-bit constant.
`c: const<_>` leaves the width unconstrained.
The angle brackets accept a literal or a name, not a compound expression.

In a term position, `c` refers to the matched value. In a supported scalar
expression, `c` supplies that constant's numeric value. An arbitrary value
binder cannot be used for scalar arithmetic in generated instcombine rules.

### Type groups

A group gives a name to type alternatives:

```pdl
group Word = i32 | i64;
rule grouped-add-zero: builtin.addi(x: Word, 0) => x;
```

Group names must be unique in a compiled input. Group declarations describe
type alternatives, not collections of rules. The frontend preserves them,
but the Rust generator does not expand or match named groups.

## Constants and scalar expressions

A literal term on the left matches a constant. A width-limited constructor
has the form `const<WIDTH>(EXPRESSION)`.

```pdl
rule mul-zero: builtin.muli(x: int<W>, 0) => const<W>(0);
rule add-constants:
	builtin.addi(a: const<W>, b: const<W>) => const<W>(a + b);
```

For generated instcombine rules, the right-hand constructor accepts widths
from 1 through 64. A dynamic width outside this range rejects the match.
Constant construction truncates the value to the requested width.

On the generated-rule left-hand side, constant patterns use plain nonnegative
literals or constant binders such as `c: const<W>`. Unary negative terms,
computed expressions, and `const<W>(...)` constructors can parse there but
are not supported by Rust lowering.

The scalar expression `a + b` computes a constant while applying the rule.
The term `builtin.addi(a, b)` describes a program operation. The distinction
also applies to `#add(a, b)`, which is a semantic operation term.

### Operators and precedence

The table lists precedence from highest to lowest. Binary operators at the
same level associate left to right. Parentheses override precedence.

| Level | Operators | Meaning |
| --- | --- | --- |
| 1 | Unary `-`, `!` | Negation, logical not. |
| 2 | `*`, `/`, `%` | Multiplication, division, remainder. |
| 3 | `+`, `-` | Addition, subtraction. |
| 4 | `<<`, `>>` | Shifts. |
| 5 | `<`, `<=`, `>`, `>=` | Ordered comparisons. |
| 6 | `==`, `!=` | Equality comparisons. |
| 7 | `&` | Bitwise and. |
| 8 | `^`, `\|` | Bitwise xor and or, at the same precedence. |
| 9 | `&&`, `\|\|` | Logical and and or, at the same precedence. |

These levels differ from C. In particular, `A || B && C` groups as
`(A || B) && C`. Comparisons bind more tightly than bitwise operations, so a
bit test is written `(c & 1) == 0`.

In an operation's operand list, `|` separates dependencies. Bitwise-or scalar
expressions belong in a `where` clause or inside `const<...>(...)`.
There is no unary bitwise-complement token. `!` means logical not.

### Arithmetic in generated rules

Guards evaluate numeric expressions using signed 64-bit host arithmetic.
Constant binders provide their stored bit pattern: a narrow all-ones constant
is a positive mask, not automatically a sign-extended negative number.
A 64-bit pattern is interpreted as a signed 64-bit host value.

Overflow in checked arithmetic, division by zero, and invalid shift counts
reject the match. Right shift uses signed host arithmetic. A numeric guard is
true when its value is nonzero. Boolean expressions are not numeric values
for constant construction.

Inside a right-hand `const<W>(...)`, binary `+`, `-`, and `*` use arithmetic
modulo `2^W`. Other operators use checked host arithmetic before truncation.
An expression beneath one of those other operators also uses host arithmetic.
For example, `const<W>((a + b) / 2)` checks the addition before division;
it does not first wrap `a + b` at width `W`.

These are the generated-rule evaluator's rules. Semantic operation terms use
the meaning of their operators, including their own signedness and width.

### Expression functions

Function-call syntax is `name(argument, ...)`. There is no mechanism to define
functions in a PDL file. A consumer determines which functions it supports.

| Function | Meaning | Consumer |
| --- | --- | --- |
| `popcount(c)` | Number of set bits in constant `c`. | Instcombine Rust generation. |
| `ctz(c)` | Number of trailing zero bits. For zero, the constant's width. | Instcombine Rust generation. |
| `clz(c)` | Number of leading zero bits within the constant's width. For zero, that width. | Instcombine Rust generation. |
| `ones(W)` | A mask with `W` low bits set. | Semantic axioms and their width expressions. |
| `fits(c, N)` | Whether the constant fits in a signed `N`-bit field. | Semantic axiom guards. |
| `ufits(c, N)` | Whether the constant fits in an unsigned `N`-bit field. | Semantic axiom guards. |
| `materializable(c)` | Whether one of the target's instructions produces the constant alone. Without a target, every constant qualifies. | Semantic axiom guards. |

The bit-count functions take one constant binder directly. Expressions such
as `popcount(c + 1)` do not preserve a binder width and are rejected by the
Rust generator. `fits` and `ufits` take a bound constant and a literal field
width from 1 through 64. Their negations, such as `!fits(c, 12)`, are supported.
`materializable` takes a bound constant and may also be negated.

Semantic axiom guards support width comparisons with `<` and `==`, and the
`fits` family. They do not support the whole generated-rule expression
language. Comma-separated guards express conjunction in both consumers.

## Root references and constant materialization

`root` refers to the equivalence class matched by the entire left-hand side.
It is valid only on the right. A semantic rule can use it to describe an
alternative that refers back to that class. Such a rule can create cycles
in the e-graph; it does not recursively copy the original syntax tree.
The semantic axiom loader permits RHS references to `root` or to bound
values, but not both in the same replacement.

A rule with a bare constant binder on the left is a materialization rule.
It describes how to compute a constant using operations, such as when the
constant does not fit a machine instruction's immediate field.

```pdl
rule wide-constant:
	v: const<W>
	=> keep #add(
		keep #shl(#ashr(#sub(v, #ashr(#shl(v, W - 12), W - 12)), 12), 12),
		#ashr(#shl(v, W - 12), W - 12)
	)
	where !fits(v, 12);
```

This semantic materialization example separates a wide constant into a high
part and a signed low part. `keep` marks the outer addition and shift as
operations to retain. Unmarked constant computations can fold into the
smaller constants that those operations use.

`keep` must wrap an operation and is valid only on the right of a
materialization rule. It does not mean that an arbitrary IR operation must
survive dead-code elimination. `root`, `keep`, and semantic operation emission
are not supported by the instcombine Rust generator.

## Proof modes

A proof clause records the justification required by a rule's consumer.
Parsing a clause does not perform a proof.

| Mode | Meaning |
| --- | --- |
| `proof smt` | A solver-checkable equality obligation. Supported consumers can check it through SMT, satisfiability modulo theories. |
| `proof trusted` | An asserted equality without a solver proof. |
| `proof definitional` | A law supplied by the modeled algebra, such as a state law outside the scalar solver's model. |
| `proof contract` | A directional refinement checked against an explicit contract. Not valid for an equality rule. |

Floating-point rules default to `smt`. Other rules naming concrete dialect
operations default to `trusted`. Rules using only the semantic vocabulary
default to `smt`. Floating-point detection includes floating-point types,
operations, and floating-point environment state.

The Rust generator does not prove rules. In particular, it rejects
floating-point equalities that require SMT proof because it has no checked
proof binding. An explicit trusted declaration is an assertion of correctness,
not a substitute for establishing that correctness.

The proof tool distinguishes proven, admitted, disproven, and unsupported
obligations. A proof at one integer width is not a proof for every width.
Memory and floating-point effects can require a model beyond scalar equality.

## Directional refinements

A refinement permits a replacement under a contract without asserting
unconditional equality:

```pdl
refinement contract-mul-add:
	fp.add(fp.mul(a: float<64>, b: float<64>), c: float<64>)
	~> fp.fma(a, b, c)
	requires same_round_region, permits(contract_mul_add),
		compatible_formats_and_rounding, permitted_effect_change,
		satisfies_domain_and_effect_contract
	proof contract;
```

Fusing a multiplication and addition changes rounding. The contract determines
whether that directional change is permitted. It does not license merging both
forms into an unconditional equivalence class.

The declaration form is:

```text
refinement NAME: LEFT ~> RIGHT
    requires REQUIREMENT, REQUIREMENT
    proof MODE;
```

`requires` and `proof` are optional syntactically and appear in that order.
Refinements have no `where` or `phase` clause. Consumers still require the
appropriate checked admission; omitting a clause does not create permission.
`proof trusted` is invalid for a refinement.

The recognized requirements are:

| Requirement | Contract fact |
| --- | --- |
| `same_round_region` | The operations belong to the same rounding region. |
| `compatible_formats_and_rounding` | Formats and rounding modes are compatible with the replacement. |
| `permitted_effect_change` | The contract permits the change in effects. |
| `satisfies_domain_and_effect_contract` | The candidate meets both the value-domain and effect contract. |
| `permits(contract_mul_add)` | Permission to contract multiplication and addition. |
| `permits(cross_statement)` | Permission to combine across statements. |
| `permits(reassociation)` | Permission to reassociate. |
| `permits(reciprocal)` | Permission to use a reciprocal transformation. |
| `permits(approved_approximation)` | Permission to use an approved approximation. |

The frontend preserves these requirements for a refinement consumer. The
instcombine Rust generator rejects refinement declarations rather than
turning them into equality rewrites.

The current contraction checker requires all five requirements shown in the
example. Recognizing a permission name such as `reciprocal` does not imply
that a consumer implements that class of refinement.

## Saturation phases

Without a phase clause, a rule participates in repeated saturation rounds.
`phase post-saturation` puts it in a terminal phase that runs after those
rounds. There are no other named phases.

```pdl
rule reassociate-constants:
	builtin.addi(builtin.addi(x: int<W>, a: const), b: const)
	=> builtin.addi(x, const<W>(a + b))
	phase post-saturation;
```

The terminal phase searches the graph produced by saturation. Its results
do not feed back into ordinary rounds during that saturation call. This
controls rules whose new constants could otherwise keep generating more
forms through a cyclic equivalence class. It is a scheduling choice, not a
weaker correctness requirement.

## Consumer support

The following boundaries matter when a rule parses successfully but cannot
be used by the intended engine.

| Consumer | Supported role | Main boundaries |
| --- | --- | --- |
| PDL frontend | Parse declarations and check names, semantic operator arities, and structural restrictions. | Does not prove equalities or establish that dialect operations exist. |
| Instcombine Rust generator | Forward equality rules with relational matching and typed IR construction. | Known concrete operations; at most one new RHS operation; no attributes, type groups, refinements, or bidirectional generation. |
| Semantic axiom loader | Semantic expressions for selection and proof, including nested RHS terms, explicit widths, `root`, and materialization. | Its scalar expression and guard language is narrower; concrete dialect terms require an explicitly supported semantic binding. |
| Refinement consumer | Contract-governed directional candidates. | Separate admission and application; never automatic equality saturation. |

For the Rust generator, a RHS is a bound value, `const<W>(...)`, or one
supported concrete operation over bound values and constant constructors.
The new operation inherits the matched root's result type. It cannot emit
nested operations, explicit RHS result types, attributes, or dependency
operands. A matched operation need not have an emitter if the rule only
returns an existing value.

Integer binder lowering currently uses `int<W>`. Concrete and wildcard
integer binder annotations are accepted by the frontend but do not lower
through that path. Constant binders support explicit widths. Scalar float
and known state-resource binders have dedicated matching support; shaped
float binders and arbitrary state resources do not.

Semantic operators can appear in generated patterns when the host graph uses
that vocabulary, as with `#if`. The Rust generator cannot construct arbitrary
semantic operators on the RHS. Unknown expression functions can pass the
frontend's name checks but still fail consumer lowering.

Scalar integer proof rules normally declare a root result width and the widths
of their bound values. A constant binder needs an explicit width, and
`int<_>` supplies no width for a proof. Arbitrary type groups, shaped floats,
and state binders are outside the scalar prover's supported type model.

## Compiler commands and diagnostics

The command-line frontend accepts an input path, an output mode, and an optional
output path:

```sh
cargo run -p tir-pdl -- rules.pdl --emit ast
cargo run -p tir-pdl -- rules.pdl --emit rust -o rules.rs
cargo run -p tir-pdl -- rules.pdl --emit tokens
```

`ast` parses and checks the source. `rust` also checks the generated-rewrite
subset and emits Rust; it is the default mode. `tokens` only lexes the input.
Without `-o`, output goes to standard output. Errors include source locations
and produce a failing exit status.

Generated Rust expects the host ruleset's types, operation mappings, and
emitters. It is not a standalone Rust program. Compiling a separate file also
does not register its rules with an optimization pass.

The proof command checks supported obligations at a selected width:

```sh
cargo run -p tir-tools -- prove rules.pdl --width 32
```

Its default width is 8. `--allow-admitted` permits admitted obligations in the
command's success status; admission is distinct from solver proof.

Common diagnostics have the following meanings:

| Diagnostic | Cause |
| --- | --- |
| Duplicate rule or group | A name is declared twice in the compiled input. |
| Unbound name | The RHS or a guard refers to a name absent from the LHS bindings. |
| Type on repeated binder | A later occurrence repeats a type annotation. |
| RHS binders cannot introduce types | A RHS reference has a type annotation. |
| Unknown semantic operator or wrong arity | A `#name` or its operand count does not match the semantic vocabulary. |
| Failed to lower a rule | The consumer cannot implement a construct accepted by the frontend. |
| Cannot emit operation | Matching support exists, but no typed constructor is available for the RHS operation. |
| Floating-point equality requires a checked proof binding | The generated-rule path cannot discharge the requested proof. |

Every complete `pdl` code block in this chapter is a standalone frontend
example. Consumer-specific restrictions still apply. The repository's
`utils/pdl/scripts/check-doc-examples.py` checks those blocks with the frontend.

```sh
cargo build -p tir-pdl
python3 utils/pdl/scripts/check-doc-examples.py
```
