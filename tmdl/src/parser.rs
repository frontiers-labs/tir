use chumsky::{input::ValueInput, prelude::*};

use crate::{
    Span, Spanned, Type,
    ast::{self, *},
    lexer::Token,
};

pub fn parse<'src>(
    source: &'src str,
    tokens: &'src [Spanned<Token>],
    file_name: &str,
) -> (Option<File>, Vec<Rich<'src, Token<'src>, Span>>) {
    file(file_name)
        .then_ignore(end())
        .parse(tokens.map((source.len()..source.len()).into(), |(t, s)| (t, s)))
        .into_output_errors()
}

/// Parse single translation unit
fn file<'src, I>(
    file_name: &str,
) -> impl Parser<'src, I, File, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let fname = file_name.to_string();
    choice((
        isa_def().map(Item::Isa),
        register_class_def().map(Item::RegisterClass),
        template_def().map(Item::Template),
        instruction_def().map(Item::Instruction),
        unit_def().map(Item::Unit),
        machine_def().map(Item::Machine),
    ))
    .repeated()
    .at_least(0)
    .collect()
    .map(move |items| File {
        items,
        file_name: fname.clone(),
    })
}

/// Parse isa definition.
/// Example:
///
/// ```tmdl
/// isa RV32I {
///   XLEN = 32,
/// }
/// ```
fn isa_def<'src, I>() -> impl Parser<'src, I, Isa, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    enum IsaBodyItem {
        Param((String, (Type, Option<Expr>))),
        Trap(TrapHandler),
    }
    let trap = just(Token::Identifier("trap"))
        .ignore_then(
            ident()
                .separated_by(just(Token::Comma))
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        )
        .then(expr())
        .map_with(|(params, body), e| TrapHandler {
            params: params.unwrap_or_default(),
            body,
            span: e.span(),
        });
    just(Token::KwIsa)
        .ignore_then(ident())
        .then(isa_requirements())
        .then_ignore(just(Token::LBrace))
        .then(
            choice((
                parameter().map(IsaBodyItem::Param),
                trap.map(IsaBodyItem::Trap),
            ))
            .repeated()
            .collect::<Vec<_>>(),
        )
        .then_ignore(just(Token::RBrace))
        .map_with(|((name, requires), items), e| {
            let mut parameters = crate::utils::StableHashMap::default();
            let mut trap_handler = None;
            for item in items {
                match item {
                    IsaBodyItem::Param((name, value)) => {
                        parameters.insert(name, value);
                    }
                    IsaBodyItem::Trap(t) => trap_handler = Some(t),
                }
            }
            Isa {
                name,
                requires,
                parameters,
                trap_handler,
                span: e.span(),
            }
        })
        .labelled("ISA definition")
}

/// Register class definition
///
/// Example:
/// ```tmdl
/// register_class GPR for TestIsa {
///   parameters {
///     width = self.XLEN,
///     encoding_len = 5,
///   }
///   registers {
///     x0("zero") => { traits = [hardwired_zero] },
///     x1("ra") => { traits = [return_address, caller_saved] },
///     x2..x31("r{}") => { traits = [ callee_saved ] },
///   }
/// }
/// ```
fn register_class_def<'src, I>()
-> impl Parser<'src, I, RegisterClass, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };
    just(Token::KwRegClass)
        .ignore_then(ident)
        .then(for_isas())
        .then(just(Token::Colon).ignore_then(ident).or_not())
        .then(
            choice((
                parameter().map(RegClassBody::Param),
                register_class_registers().map(RegClassBody::Registers),
            ))
            .repeated()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|(((name, for_isas), base), body), e| {
            let parameters = body
                .iter()
                .filter_map(|b| match b {
                    RegClassBody::Param(p) => Some(p.clone()),
                    _ => None,
                })
                .collect();

            let registers = body
                .iter()
                .find_map(|b| {
                    if let RegClassBody::Registers(r) = b {
                        Some(r.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            RegisterClass {
                name,
                for_isas,
                base,
                parameters,
                registers,
                span: e.span(),
            }
        })
        .labelled("register class definition")
}

enum RegClassBody {
    Param((String, (Type, Option<ast::Expr>))),
    Registers(Vec<RegisterDef>),
}

fn template_def<'src, I>()
-> impl Parser<'src, I, Template, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };

    just(Token::KwTemplate)
        .ignore_then(ident)
        .then(for_isas().or_not())
        .then(just(Token::Colon).ignore_then(ident).or_not())
        .then(
            choice((
                parameter().map(TemplateOrInstBody::Param),
                instruction_operands().map(TemplateOrInstBody::Operands),
                encoding().map(TemplateOrInstBody::Encoding),
                asm().map(TemplateOrInstBody::Asm),
                schedule().map(TemplateOrInstBody::Schedule),
            ))
            .repeated()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|(((name, for_isas), parent_template), body), e| {
            let params = body
                .iter()
                .filter_map(|b| match b {
                    TemplateOrInstBody::Param(p) => Some(p.clone()),
                    _ => None,
                })
                .collect();

            let operands = body
                .iter()
                .find_map(|b| {
                    if let TemplateOrInstBody::Operands(o) = b {
                        Some(o.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let encoding = body
                .iter()
                .find_map(|b| {
                    if let TemplateOrInstBody::Encoding(e) = b {
                        Some(e.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let asm = body.iter().find_map(|b| {
                if let TemplateOrInstBody::Asm(a) = b {
                    Some(a.clone())
                } else {
                    None
                }
            });

            let schedule = body.iter().find_map(|b| {
                if let TemplateOrInstBody::Schedule(t) = b {
                    Some(t.clone())
                } else {
                    None
                }
            });

            Template {
                name,
                for_isas: for_isas.unwrap_or_default(),
                parent_template,
                params,
                operands,
                encoding,
                asm,
                schedule,
                span: e.span(),
            }
        })
}

fn instruction_def<'src, I>()
-> impl Parser<'src, I, Instruction, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };

    just(Token::KwInstruction)
        .ignore_then(ident)
        .then(for_isas().or_not())
        .then(just(Token::Colon).ignore_then(ident).or_not())
        .then(
            choice((
                parameter().map(TemplateOrInstBody::Param),
                instruction_operands().map(TemplateOrInstBody::Operands),
                encoding().map(TemplateOrInstBody::Encoding),
                asm().map(TemplateOrInstBody::Asm),
                behavior().map(TemplateOrInstBody::Behavior),
                schedule().map(TemplateOrInstBody::Schedule),
            ))
            .repeated()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|(((name, for_isas), parent_template), body), e| {
            let params = body
                .iter()
                .filter_map(|b| match b {
                    TemplateOrInstBody::Param(p) => Some(p.clone()),
                    _ => None,
                })
                .collect();

            let operands = body
                .iter()
                .find_map(|b| {
                    if let TemplateOrInstBody::Operands(o) = b {
                        Some(o.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let encoding = body
                .iter()
                .find_map(|b| {
                    if let TemplateOrInstBody::Encoding(e) = b {
                        Some(e.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let asm = body.iter().find_map(|b| {
                if let TemplateOrInstBody::Asm(a) = b {
                    Some(a.clone())
                } else {
                    None
                }
            });

            let behavior = body
                .iter()
                .find_map(|b| {
                    if let TemplateOrInstBody::Behavior(a) = b {
                        Some(a.clone())
                    } else {
                        None
                    }
                })
                .unwrap();

            let schedule = body.iter().find_map(|b| {
                if let TemplateOrInstBody::Schedule(t) = b {
                    Some(t.clone())
                } else {
                    None
                }
            });

            Instruction {
                name,
                for_isas: for_isas.unwrap_or_default(),
                parent_template,
                params,
                operands,
                encoding,
                asm,
                behavior,
                schedule,
                span: e.span(),
            }
        })
        .labelled("instruction definition")
}

enum TemplateOrInstBody {
    Param((String, (Type, Option<ast::Expr>))),
    Operands(Vec<(String, Type)>),
    Encoding(Vec<EncodingArm>),
    Asm(Expr),
    Behavior(Expr),
    Schedule(Schedule),
}

fn asm<'src, I>() -> impl Parser<'src, I, Expr, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    just(Token::KwAsm).ignore_then(expr())
}

fn behavior<'src, I>() -> impl Parser<'src, I, Expr, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    just(Token::KwBehavior).ignore_then(expr())
}

/// Parse an integer literal token (`42`, `0x10`, `0b101`) into an `i64`.
fn int_lit<'src, I>() -> impl Parser<'src, I, i64, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    select! { Token::Number(n) => n }.try_map_with(|n, e| {
        parse_int_lit(n).ok_or_else(|| Rich::custom(e.span(), "invalid integer literal"))
    })
}

fn parse_int_lit(n: &str) -> Option<i64> {
    if let Some(h) = n.strip_prefix("0x").or_else(|| n.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()
    } else if let Some(b) = n.strip_prefix("0b").or_else(|| n.strip_prefix("0B")) {
        i64::from_str_radix(b, 2).ok()
    } else {
        n.parse::<i64>().ok()
    }
}

/// Parse a bracketed, comma-separated list of identifiers: `[a, b, c]`.
fn ident_list<'src, I>()
-> impl Parser<'src, I, Vec<String>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(i) => i.to_string() };
    ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect()
        .delimited_by(just(Token::LBracket), just(Token::RBracket))
}

/// Instruction `schedule { units = [..]; }` block.
fn schedule<'src, I>() -> impl Parser<'src, I, Schedule, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    just(Token::KwSchedule)
        .ignore_then(
            just(Token::Identifier("units"))
                .ignore_then(just(Token::Equals))
                .ignore_then(ident_list())
                .then_ignore(just(Token::Semicolon))
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|units, e| Schedule {
            classes: units,
            span: e.span(),
        })
        .labelled("schedule block")
}

#[derive(Clone)]
enum UnitField {
    Latency(i64),
    Throughput(i64),
}

/// A `name = N;` integer pair, used inside `buffers` and `resource` bodies.
fn kv_int_pair<'src, I>()
-> impl Parser<'src, I, (String, i64), extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(i) => i.to_string() };
    ident
        .then_ignore(just(Token::Equals))
        .then(int_lit())
        .then_ignore(just(Token::Semicolon))
}

/// Top-level `unit Name;` or `unit Name { latency = N; throughput = N; }`.
fn unit_def<'src, I>()
-> impl Parser<'src, I, SchedClassDecl, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let field = choice((
        just(Token::Identifier("latency"))
            .ignore_then(just(Token::Equals))
            .ignore_then(int_lit())
            .then_ignore(just(Token::Semicolon))
            .map(UnitField::Latency),
        just(Token::Identifier("throughput"))
            .ignore_then(just(Token::Equals))
            .ignore_then(int_lit())
            .then_ignore(just(Token::Semicolon))
            .map(UnitField::Throughput),
    ));
    let body = field
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace));

    just(Token::KwSchedClass)
        .ignore_then(ident())
        .then(choice((
            body.map(Some),
            just(Token::Semicolon).to(None::<Vec<UnitField>>),
        )))
        .map_with(|(name, fields), e| {
            let mut default_latency = None;
            let mut default_throughput = None;
            for f in fields.unwrap_or_default() {
                match f {
                    UnitField::Latency(v) => default_latency = Some(v),
                    UnitField::Throughput(v) => default_throughput = Some(v),
                }
            }
            SchedClassDecl {
                name,
                default_latency,
                default_throughput,
                span: e.span(),
            }
        })
        .labelled("unit declaration")
}

#[derive(Clone)]
enum BindField {
    Latency(i64),
    Throughput(i64),
    Uses(Vec<String>),
    Reads(String),
    Writes(String),
}

/// The timing fields common to a `bind` and an `override` body.
#[derive(Default)]
struct BindFields {
    latency: Option<i64>,
    throughput: Option<i64>,
    reads: Option<String>,
    writes: Option<String>,
    uses: Vec<String>,
}

fn aggregate_bind_fields(fields: Vec<BindField>) -> BindFields {
    let mut out = BindFields::default();
    for f in fields {
        match f {
            BindField::Latency(v) => out.latency = Some(v),
            BindField::Throughput(v) => out.throughput = Some(v),
            BindField::Uses(u) => out.uses = u,
            BindField::Reads(p) => out.reads = Some(p),
            BindField::Writes(p) => out.writes = Some(p),
        }
    }
    out
}

/// One `latency`/`throughput`/`uses`/`reads`/`writes` field of a `bind` or
/// `override` body.
fn bind_field<'src, I>()
-> impl Parser<'src, I, BindField, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(i) => i.to_string() };
    choice((
        just(Token::Identifier("latency"))
            .ignore_then(just(Token::Equals))
            .ignore_then(int_lit())
            .then_ignore(just(Token::Semicolon))
            .map(BindField::Latency),
        just(Token::Identifier("throughput"))
            .ignore_then(just(Token::Equals))
            .ignore_then(int_lit())
            .then_ignore(just(Token::Semicolon))
            .map(BindField::Throughput),
        just(Token::Identifier("uses"))
            .ignore_then(just(Token::Equals))
            .ignore_then(ident_list())
            .then_ignore(just(Token::Semicolon))
            .map(BindField::Uses),
        just(Token::Identifier("reads"))
            .ignore_then(just(Token::Equals))
            .ignore_then(ident)
            .then_ignore(just(Token::Semicolon))
            .map(BindField::Reads),
        just(Token::Identifier("writes"))
            .ignore_then(just(Token::Equals))
            .ignore_then(ident)
            .then_ignore(just(Token::Semicolon))
            .map(BindField::Writes),
    ))
}

/// A braced `bind`/`override` body: zero or more timing fields.
fn bind_body<'src, I>()
-> impl Parser<'src, I, Vec<BindField>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    bind_field()
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
}

#[derive(Clone)]
enum MachineBody {
    IssueWidth(i64),
    Buffers(Vec<(String, i64)>),
    Pipeline(Vec<PipelinePhase>),
    Override(MachineOverride),
    Forward(Forward),
    Resource(MachineUnit),
    Bind(UnitBind),
    RegFiles(Vec<(String, i64)>),
}

/// Parse a pipeline-stage protection mode identifier into [`Protection`].
fn protection_mode<'src, I>()
-> impl Parser<'src, I, Protection, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    select! { Token::Identifier(i) => i }.try_map_with(|i, e| match i {
        "protected" => Ok(Protection::Protected),
        "unprotected" => Ok(Protection::Unprotected),
        "hard" => Ok(Protection::Hard),
        other => Err(Rich::custom(
            e.span(),
            format!("unknown protection mode '{other}' (expected protected, unprotected, or hard)"),
        )),
    })
}

/// Parse one pipeline phase: `NAME;` or `NAME: <protection>;`.
fn pipeline_phase<'src, I>()
-> impl Parser<'src, I, PipelinePhase, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(i) => i.to_string() };
    ident
        .then(just(Token::Colon).ignore_then(protection_mode()).or_not())
        .then_ignore(just(Token::Semicolon))
        .map_with(|(name, protection), e| PipelinePhase {
            name,
            protection: protection.unwrap_or(Protection::Protected),
            span: e.span(),
        })
}

/// Top-level `machine Name for [..] { issue_width=..; buffers{..} resource X{..} bind Y{..} }`.
fn machine_def<'src, I>() -> impl Parser<'src, I, Machine, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(i) => i.to_string() };

    let issue_width = just(Token::Identifier("issue_width"))
        .ignore_then(just(Token::Equals))
        .ignore_then(int_lit())
        .then_ignore(just(Token::Semicolon))
        .map(MachineBody::IssueWidth);

    let buffers = just(Token::KwBuffers)
        .ignore_then(
            kv_int_pair()
                .repeated()
                .collect::<Vec<(String, i64)>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(MachineBody::Buffers);

    let pipeline = just(Token::KwPipeline)
        .ignore_then(
            pipeline_phase()
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(MachineBody::Pipeline);

    let resource = just(Token::KwUnit)
        .ignore_then(ident)
        .then(
            kv_int_pair()
                .repeated()
                .collect::<Vec<(String, i64)>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|(name, fields), e| {
            let units = fields
                .iter()
                .find(|(k, _)| k == "count")
                .map(|(_, v)| *v)
                .unwrap_or(1);
            MachineBody::Resource(MachineUnit {
                name,
                units,
                span: e.span(),
            })
        });

    // `reg_file { GPR { count = 128; } FPR { count = 96; } }` — physical register
    // file sizes for renaming, keyed by physical-file name.
    let reg_file_entry = ident
        .then(
            kv_int_pair()
                .repeated()
                .collect::<Vec<(String, i64)>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|(name, fields): (String, Vec<(String, i64)>)| {
            let count = fields
                .iter()
                .find(|(k, _)| k == "count")
                .map(|(_, v)| *v)
                .unwrap_or(0);
            (name, count)
        });
    let reg_file = just(Token::KwRegFile)
        .ignore_then(
            reg_file_entry
                .repeated()
                .collect::<Vec<(String, i64)>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(MachineBody::RegFiles);

    let bind = just(Token::KwBind)
        .ignore_then(ident)
        .then(bind_body())
        .map_with(|(unit, fields), e| {
            let f = aggregate_bind_fields(fields);
            MachineBody::Bind(UnitBind {
                unit,
                latency: f.latency,
                throughput: f.throughput,
                reads: f.reads,
                writes: f.writes,
                uses: f.uses,
                span: e.span(),
            })
        });

    let r#override = just(Token::KwOverride)
        .ignore_then(ident)
        .then(bind_body())
        .map_with(|(instruction, fields), e| {
            let f = aggregate_bind_fields(fields);
            MachineBody::Override(MachineOverride {
                instruction,
                latency: f.latency,
                throughput: f.throughput,
                reads: f.reads,
                writes: f.writes,
                uses: f.uses,
                span: e.span(),
            })
        });

    // `forward FROM => TO { latency = N; }`
    let forward = just(Token::KwForward)
        .ignore_then(ident)
        .then_ignore(just(Token::FatArrow))
        .then(ident)
        .then(
            just(Token::Identifier("latency"))
                .ignore_then(just(Token::Equals))
                .ignore_then(int_lit())
                .then_ignore(just(Token::Semicolon))
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|((from, to), latency), e| {
            MachineBody::Forward(Forward {
                from,
                to,
                latency,
                span: e.span(),
            })
        });

    let machine_alias = just(Token::LParen)
        .ignore_then(select! { Token::StringLit(s) => s.to_string() })
        .then_ignore(just(Token::RParen))
        .or_not();

    just(Token::KwMachine)
        .ignore_then(ident)
        .then(machine_alias)
        .then(for_isas())
        .then(
            choice((
                issue_width,
                buffers,
                pipeline,
                r#override,
                forward,
                resource,
                reg_file,
                bind,
            ))
            .repeated()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|(((name, alias), for_isas), body), e| {
            let mut issue_width = None;
            let mut buffers = Vec::new();
            let mut pipeline = Vec::new();
            let mut resources = Vec::new();
            let mut reg_files = Vec::new();
            let mut binds = Vec::new();
            let mut overrides = Vec::new();
            let mut forwards = Vec::new();
            for b in body {
                match b {
                    MachineBody::IssueWidth(v) => issue_width = Some(v),
                    MachineBody::Buffers(v) => buffers = v,
                    MachineBody::Pipeline(v) => pipeline = v,
                    MachineBody::Resource(r) => resources.push(r),
                    MachineBody::RegFiles(rf) => reg_files = rf,
                    MachineBody::Bind(bd) => binds.push(bd),
                    MachineBody::Override(ov) => overrides.push(ov),
                    MachineBody::Forward(fw) => forwards.push(fw),
                }
            }
            Machine {
                name,
                alias,
                for_isas,
                issue_width,
                buffers,
                pipeline,
                resources,
                reg_files,
                binds,
                overrides,
                forwards,
                span: e.span(),
            }
        })
        .labelled("machine definition")
}

fn encoding<'src, I>()
-> impl Parser<'src, I, Vec<EncodingArm>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let num = select! { Token::Number(i) => i.parse::<u16>().unwrap() };

    let single_bit = num
        .then_ignore(just(Token::FatArrow))
        .then(inline_expr())
        .map_with(|(start, value), e| EncodingArm {
            start,
            end: None,
            value,
            span: e.span(),
        });
    let range = num
        .then_ignore(just(Token::Range))
        .then(num)
        .then_ignore(just(Token::FatArrow))
        .then(inline_expr())
        .map_with(|((start, end), value), e| EncodingArm {
            start,
            end: Some(end),
            value,
            span: e.span(),
        });
    just(Token::KwEncoding)
        .ignored()
        .then(
            choice((single_bit, range))
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|((), arms)| arms)
}

fn parameter<'src, I>()
-> impl Parser<'src, I, (String, (Type, Option<ast::Expr>)), extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };
    just(Token::KwParam)
        .ignored()
        .then(ident)
        .then_ignore(just(Token::Colon))
        .then(type_())
        .then(just(Token::Equals).then(inline_expr()).or_not())
        .then_ignore(just(Token::Semicolon))
        .map(|((((), name), ty), expr)| {
            let expr = expr.map(|e| e.1);
            (name, (ty, expr))
        })
}

fn instruction_operands<'src, I>()
-> impl Parser<'src, I, Vec<(String, Type)>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(i) => i.to_string() };
    let single_operand = ident.then_ignore(just(Token::Colon)).then(type_());
    just(Token::KwOperands)
        .ignored()
        .then(
            single_operand
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|((), operands)| operands)
}

fn isa_requirements<'src, I>()
-> impl Parser<'src, I, Option<IsaRequirement>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };
    let single_isa =
        select! { Token::Identifier(ident) => IsaRequirement::Single(ident.to_string()) };
    let any = ident
        .separated_by(just(Token::Pipe))
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBracket), just(Token::RBracket))
        .map(IsaRequirement::Any);
    let all = ident
        .separated_by(just(Token::Comma))
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBracket), just(Token::RBracket))
        .map(IsaRequirement::All);
    just(Token::KwRequires)
        .ignored()
        .then(choice((single_isa, any, all)))
        .or_not()
        .map(|isa| isa.map(|(_, isa)| isa))
}

fn for_isas<'src, I>()
-> impl Parser<'src, I, Vec<String>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };
    just(Token::KwFor)
        .ignored()
        .then(
            ident
                .separated_by(just(Token::Comma))
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBracket), just(Token::RBracket)),
        )
        .map(|(_, isas)| isas)
        .or_not()
        .map(|isas_opt| isas_opt.unwrap_or_default())
}

fn register_class_registers<'src, I>()
-> impl Parser<'src, I, Vec<RegisterDef>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    just(Token::KwRegisters)
        .ignored()
        .then(
            single_register()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|((), v)| v)
        .labelled("register class registers")
}

fn single_register<'src, I>()
-> impl Parser<'src, I, RegisterDef, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };
    let alias = just(Token::LParen)
        .ignored()
        .then(select! { Token::StringLit(s) => s.to_string() })
        .then_ignore(just(Token::RParen))
        .map(|(_, alias)| Some(alias))
        .or_not()
        .map(|o| o.flatten());

    let reg_traits = register_traits();

    // Optional explicit encoding index: `index = 0xC00`, before any traits.
    let reg_index = just(Token::Identifier("index"))
        .then_ignore(just(Token::Equals))
        .ignore_then(select! { Token::Number(n) => n.to_string() })
        .then_ignore(just(Token::Comma).or_not())
        .or_not();

    let single = ident
        .then(alias)
        .then_ignore(just(Token::FatArrow))
        .then_ignore(just(Token::LBrace))
        .then(reg_index)
        .then(reg_traits.or_not())
        .then_ignore(just(Token::RBrace))
        .map_with(|(((name, alias), index), traits), e| {
            let index = index
                .map(|n| crate::utils::parse_literal_value(&ast::LitInt::new(n, e.span())) as u16);
            RegisterDef::Single(Register {
                name,
                alias,
                index,
                traits: traits.unwrap_or_default(),
                subregisters: Vec::new(),
                span: e.span(),
            })
        });

    let range = register_range();

    choice((range, single)).labelled("register")
}

fn ident<'src, I>() -> impl Parser<'src, I, String, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    any().filter(is_ident).map(|t| t.as_ident().to_string())
}

fn register_traits<'src, I>()
-> impl Parser<'src, I, Vec<RegisterTrait>, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    just(Token::Identifier("traits"))
        .then_ignore(just(Token::Equals))
        .then_ignore(just(Token::LBracket))
        .ignore_then(
            select! { Token::Identifier(t) => t.to_string() }
                .separated_by(just(Token::Comma))
                .collect::<Vec<_>>(),
        )
        .then_ignore(just(Token::RBracket))
        .map(|traits| {
            traits
                .into_iter()
                .filter_map(|t| match t.as_str() {
                    "hardwired_zero" => Some(RegisterTrait::HardwiredZero),
                    "return_address" => Some(RegisterTrait::ReturnAddress),
                    "caller_saved" => Some(RegisterTrait::CallerSaved),
                    "callee_saved" => Some(RegisterTrait::CalleeSaved),
                    "stack_pointer" => Some(RegisterTrait::StackPointer),
                    "program_counter" => Some(RegisterTrait::ProgramCounter),
                    "global_pointer" => Some(RegisterTrait::GlobalPointer),
                    "thread_pointer" => Some(RegisterTrait::ThreadPointer),
                    "argument" => Some(RegisterTrait::Argument),
                    "return_value" => Some(RegisterTrait::ReturnValue),
                    "temporary" => Some(RegisterTrait::Temporary),
                    "saved" => Some(RegisterTrait::Saved),
                    _ => None,
                })
                .collect()
        })
}

fn register_range<'src, I>()
-> impl Parser<'src, I, RegisterDef, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(ident) => ident.to_string() };
    let alias_pattern = just(Token::LParen)
        .ignored()
        .then(select! { Token::StringLit(s) => s.to_string() })
        .then_ignore(just(Token::RParen))
        .map(|(_, alias)| Some(alias))
        .or_not()
        .map(|o| o.flatten());

    let reg_traits = register_traits();

    ident
        .then_ignore(just(Token::Range))
        .then(ident)
        .then(alias_pattern)
        .then_ignore(just(Token::FatArrow))
        .then_ignore(just(Token::LBrace))
        .then(reg_traits)
        .then_ignore(just(Token::RBrace))
        .map_with(|(((start, end), alias_pattern), traits), e| {
            RegisterDef::Range(RegisterRange {
                start,
                end,
                alias_pattern,
                traits,
                span: e.span(),
            })
        })
}

fn inline_expr<'src, I>() -> impl Parser<'src, I, Expr, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    recursive(|expr| {
        fn builtin_from_ident(name: &str) -> Option<BuiltinFunction> {
            match name {
                "clamp" => Some(BuiltinFunction::Clamp),
                "extract" => Some(BuiltinFunction::Extract),
                "log2Ceil" => Some(BuiltinFunction::Log2Ceil),
                "lane" => Some(BuiltinFunction::Lane),
                "sext" => Some(BuiltinFunction::SExt),
                "zext" => Some(BuiltinFunction::ZExt),
                "load" => Some(BuiltinFunction::Load),
                "store" => Some(BuiltinFunction::Store),
                "trap" => Some(BuiltinFunction::Trap),
                _ => None,
            }
        }

        let ident = select! { Token::Identifier(i) => i.to_string() };
        let scope = just(Token::Colon).then(just(Token::Colon));

        let ident_or_path =
            ident
                .then(scope.ignore_then(ident).or_not())
                .map_with(|(base, member), e| {
                    if let Some(member) = member {
                        Expr::Path(Path {
                            base,
                            remainder: vec![member],
                            span: e.span(),
                        })
                    } else if let Some(b) = builtin_from_ident(&base) {
                        Expr::BuiltinFunction(b)
                    } else {
                        Ident::new(base, e.span()).into()
                    }
                });

        let literal_or_ident = choice((
            ident_or_path,
            select! { Token::Number(n) => n.to_string() }
                .map_with(|n, e| LitInt::new(n, e.span()).into()),
            select! { Token::StringLit(s) => s.to_string() }
                .map_with(|s, e| LitStr::new(s, e.span()).into()),
        ))
        .labelled("value");

        let num = select! {
          Token::Number(n) => n.parse::<u16>().unwrap(),
        };

        let ident = select! { Token::Identifier(i) => i.to_string() };

        let atom = literal_or_ident
            .or(expr
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen)))
            .boxed();

        let items = expr
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>();

        // Postfix chain: field access, slice, index, then call
        #[derive(Clone)]
        enum PostfixOp {
            Field(String, Span),
            Slice(u16, u16, Span),
            Index(u16, Span),
            Call(Vec<Expr>, Span),
        }

        let postfix_op = choice((
            // field: .ident
            just(Token::Dot)
                .then(ident)
                .map_with(|(_, b), e| PostfixOp::Field(b, e.span())),
            // slice: [start..end]
            num.then_ignore(just(Token::Range))
                .then(num)
                .delimited_by(just(Token::LBracket), just(Token::RBracket))
                .map_with(|(start, end), e| PostfixOp::Slice(start, end, e.span())),
            // index: [idx]
            num.delimited_by(just(Token::LBracket), just(Token::RBracket))
                .map_with(|index, e| PostfixOp::Index(index, e.span())),
            // call: (args)
            items
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map_with(|arguments, e| PostfixOp::Call(arguments, e.span())),
        ));

        let basic = atom
            .clone()
            .foldl_with(postfix_op.repeated(), |base, op, _e| match op {
                PostfixOp::Field(member, span) => Expr::Field(Field {
                    base: Box::new(base),
                    member,
                    span,
                }),
                PostfixOp::Slice(start, end, span) => Expr::Slice(Slice {
                    base: Box::new(base),
                    start,
                    end,
                    span,
                }),
                PostfixOp::Index(index, span) => Expr::IndexAccess(IndexAccess {
                    base: Box::new(base),
                    index,
                    span,
                }),
                PostfixOp::Call(arguments, span) => Expr::Call(Call {
                    callee: Box::new(base),
                    arguments,
                    span,
                }),
            });

        let unary = just(Token::Tilde)
            .to(UnOp::BitwiseNot)
            .repeated()
            .foldr_with(basic.clone(), |op, x, e| {
                Expr::Unary(Unary {
                    x: Box::new(x),
                    op,
                    span: e.span(),
                })
            })
            .boxed();

        let op = just(Token::Asterisk)
            .to(BinOp::Mul)
            .or(just(Token::Tilde)
                .then(just(Token::ForwardSlash))
                .to(BinOp::UnsignedDiv))
            .or(just(Token::ForwardSlash).to(BinOp::Div));
        let product = unary
            .clone()
            .foldl_with(op.then(expr).repeated(), |a, (op, b), e| {
                let sp = e.span();
                Expr::Binary(Binary {
                    lhs: Box::new(a),
                    rhs: Box::new(b),
                    op,
                    span: sp,
                })
            });

        let op = choice((
            just(Token::Plus).to(BinOp::Add),
            just(Token::Dash).to(BinOp::Sub),
            just(Token::Pipe).to(BinOp::BitwiseOr),
            just(Token::Ampersand).to(BinOp::BitwiseAnd),
            just(Token::Hat).to(BinOp::BitwiseXor),
            just(Token::LAngle)
                .then(just(Token::LAngle))
                .to(BinOp::ShiftLeftLogical),
            // Prefer the longer operator first: >>> (arith) before >> (logical)
            just(Token::RAngle)
                .then(just(Token::RAngle))
                .then(just(Token::RAngle))
                .to(BinOp::ShiftRightArithmetic),
            just(Token::RAngle)
                .then(just(Token::RAngle))
                .to(BinOp::ShiftRightLogical),
        ));

        let arith = product
            .clone()
            .foldl_with(op.then(product).repeated(), |a, (op, b), e| {
                let sp = e.span();
                Expr::Binary(Binary {
                    lhs: Box::new(a),
                    rhs: Box::new(b),
                    op,
                    span: sp,
                })
            });

        let cmp_op = choice((
            just(Token::Equals)
                .then(just(Token::Equals))
                .to(BinOp::Equal),
            just(Token::Bang)
                .then(just(Token::Equals))
                .to(BinOp::NotEqual),
            just(Token::Tilde)
                .then(just(Token::LAngle))
                .then(just(Token::Equals))
                .to(BinOp::UnsignedLessThenEqual),
            just(Token::Tilde)
                .then(just(Token::RAngle))
                .then(just(Token::Equals))
                .to(BinOp::UnsignedGreaterThanEqual),
            just(Token::Tilde)
                .then(just(Token::LAngle))
                .to(BinOp::UnsignedLessThan),
            just(Token::Tilde)
                .then(just(Token::RAngle))
                .to(BinOp::UnsignedGreaterThan),
            just(Token::LAngle)
                .then(just(Token::Equals))
                .to(BinOp::LessThenEqual),
            just(Token::RAngle)
                .then(just(Token::Equals))
                .to(BinOp::GreaterThanEqual),
            just(Token::LAngle).to(BinOp::LessThan),
            just(Token::RAngle).to(BinOp::GreaterThan),
        ));

        arith
            .clone()
            .foldl_with(cmp_op.then(arith).repeated(), |a, (op, b), e| {
                let sp = e.span();
                Expr::Binary(Binary {
                    lhs: Box::new(a),
                    rhs: Box::new(b),
                    op,
                    span: sp,
                })
            })
            .labelled("inline expression")
    })
}

fn expr<'src, I>() -> impl Parser<'src, I, Expr, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ident = select! { Token::Identifier(i) => i.to_string() };
    let scope = just(Token::Colon).then(just(Token::Colon));
    let assign_target = ident
        .then(scope.ignore_then(ident).or_not())
        .map_with(|(base, member), e| {
            if let Some(member) = member {
                Expr::Path(Path {
                    base,
                    remainder: vec![member],
                    span: e.span(),
                })
            } else {
                Expr::Ident(Ident::new(base, e.span()))
            }
        })
        .boxed();

    recursive(|expr| {
        let assign = assign_target
            .clone()
            .then_ignore(just(Token::Equals))
            .then(expr.clone().or(inline_expr()))
            .map_with(|(dest, value), e| {
                Expr::Assign(Assign {
                    dest: Box::new(dest),
                    value: Box::new(value),
                    span: e.span(),
                })
            })
            .labelled("assignment");
        let stmt = expr.clone().or(assign).or(inline_expr()).boxed();

        let block = stmt
            .separated_by(just(Token::Semicolon))
            .collect::<Vec<_>>()
            .then(just(Token::Semicolon).or_not())
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map_with(|(stmts, trailing_semicolon), e| {
                let last_expr_return = trailing_semicolon.is_none() && !stmts.is_empty();
                Block {
                    stmts,
                    last_expr_return,
                    span: e.span(),
                }
                .into()
            })
            .boxed()
            .recover_with(via_parser(nested_delimiters(
                Token::LBrace,
                Token::RBrace,
                [
                    (Token::LParen, Token::RParen),
                    (Token::LBracket, Token::RBracket),
                ],
                |_| Expr::Invalid,
            )));

        let if_ = recursive(|if_| {
            just(Token::KwIf)
                .ignore_then(inline_expr())
                .then(block.clone())
                .then(
                    just(Token::KwElse)
                        .ignore_then(block.clone().or(if_))
                        .or_not(),
                )
                .map_with(|((cond, a), b), e| {
                    Expr::If(If {
                        cond: Box::new(cond),
                        then: Box::new(a),
                        else_: b.map(Box::new),
                        span: e.span(),
                    })
                })
                .boxed()
        });

        let loop_var = select! { Token::Identifier(i) => i.to_string() };
        let for_ = just(Token::KwFor)
            .ignore_then(loop_var)
            .then_ignore(just(Token::KwIn))
            .then(inline_expr())
            .then_ignore(just(Token::Range))
            .then(inline_expr())
            .then(block.clone())
            .map_with(|(((var, start), end), body), e| {
                Expr::For(For {
                    var,
                    start: Box::new(start),
                    end: Box::new(end),
                    body: Box::new(body),
                    span: e.span(),
                })
            })
            .boxed();

        let except = just(Token::KwExcept)
            .ignore_then(ident)
            .then(
                ident
                    .delimited_by(just(Token::LParen), just(Token::RParen))
                    .or_not(),
            )
            .then(block.clone())
            .map_with(|((kind, binding), body), e| ExceptClause {
                kind,
                binding,
                body,
                span: e.span(),
            });
        let try_ = just(Token::KwTry)
            .ignore_then(block.clone())
            .then(except.repeated().at_least(1).collect::<Vec<_>>())
            .map_with(|(body, handlers), e| {
                Expr::Try(TryExcept {
                    body: Box::new(body),
                    handlers,
                    span: e.span(),
                })
            })
            .boxed();

        choice((block.clone(), if_, for_, try_))
    })
}

fn type_<'src, I>() -> impl Parser<'src, I, Type, extra::Err<Rich<'src, Token<'src>, Span>>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let num = select! { Token::Number(n) => n };

    let ident = select! { Token::Identifier(i) => i.to_string() };

    let width = num
        .try_map_with(|n, e| {
            n.parse::<u16>()
                .map_err(|_| Rich::custom(e.span(), "Expected unsigned integer"))
        })
        .then_ignore(just(Token::RAngle))
        .map(Type::Bits)
        .or(inline_expr()
            .then_ignore(just(Token::RAngle))
            .map(|e| Type::BitsExpr(Box::new(e))));
    let bits = just(Token::Identifier("bits"))
        .ignored()
        .then_ignore(just(Token::LAngle))
        .then(width)
        .map(|((), bits)| bits);
    choice((
        just(Token::Identifier("String")).to(Type::String),
        just(Token::Identifier("Integer")).to(Type::Integer),
        bits,
        ident.map(Type::Struct),
    ))
}

fn is_ident(token: &Token) -> bool {
    matches!(token, Token::Identifier(_))
}

#[cfg(test)]
mod tests {
    use chumsky::Parser;
    use chumsky::prelude::*;

    use crate::{
        ast::{BinOp, Expr, UnOp},
        lexer::lexer,
    };

    use super::{
        expr, inline_expr, instruction_def, isa_def, machine_def, register_class_def, unit_def,
    };

    #[test]
    fn register_class_parses_inheritance() {
        let with_base = "register_class GPRsp for [Isa] : GPR { registers { x31(\"sp\") => { traits = [stack_pointer] }, } }";
        let (tokens, _e) = lexer().parse(with_base).into_output_errors();
        let tokens = tokens.unwrap();
        let rc = register_class_def()
            .then(end())
            .parse(
                tokens
                    .as_slice()
                    .map((with_base.len()..with_base.len()).into(), |(t, s)| (t, s)),
            )
            .output()
            .unwrap()
            .0
            .clone();
        assert_eq!(rc.base.as_deref(), Some("GPR"));

        let no_base = "register_class GPR for [Isa] { registers { x0 => { traits = [] }, } }";
        let (tokens, _e) = lexer().parse(no_base).into_output_errors();
        let tokens = tokens.unwrap();
        let rc = register_class_def()
            .then(end())
            .parse(
                tokens
                    .as_slice()
                    .map((no_base.len()..no_base.len()).into(), |(t, s)| (t, s)),
            )
            .output()
            .unwrap()
            .0
            .clone();
        assert_eq!(rc.base, None);
    }

    #[test]
    fn inheritance_merges_base_registers_and_params() {
        use crate::ast::{RegisterTrait, resolve_register_class_inheritance};

        let code = r#"
            isa Isa { param XLEN: Integer = 64; }
            register_class GPR for [Isa] {
                param ENCODING_LEN: Integer = 5;
                param WIDTH: Integer = self.XLEN;
                registers {
                    x0..x30 => { traits = [caller_saved] },
                    x31("xzr") => { traits = [hardwired_zero] },
                }
            }
            register_class GPRsp for [Isa] : GPR {
                registers {
                    x31("sp") => { traits = [stack_pointer] },
                }
            }
        "#;
        let (tokens, _e) = crate::lex(code);
        let (file, errs) = crate::parse(code, &tokens, "test");
        assert!(errs.is_empty(), "{errs:?}");
        let mut files = vec![file.unwrap()];
        resolve_register_class_inheritance(&mut files);

        let gprsp = files[0]
            .register_classes()
            .find(|c| c.name == "GPRsp")
            .unwrap();
        // Parameters are inherited from the base.
        assert!(gprsp.parameters.get("WIDTH").is_some());
        assert!(gprsp.parameters.get("ENCODING_LEN").is_some());
        // The full register file is present, with slot 31 overridden to `sp`.
        let regs: Vec<_> = gprsp.resolve_registers().collect();
        assert_eq!(regs.len(), 32);
        let r31 = regs.iter().find(|r| r.name == "x31").unwrap();
        assert_eq!(r31.alias.as_deref(), Some("sp"));
        assert!(r31.traits.contains(&RegisterTrait::StackPointer));
        // The base class is left untouched: slot 31 is still `xzr`.
        let gpr = files[0]
            .register_classes()
            .find(|c| c.name == "GPR")
            .unwrap();
        let g31 = gpr.resolve_registers().find(|r| r.name == "x31").unwrap();
        assert_eq!(g31.alias.as_deref(), Some("xzr"));
        assert!(g31.traits.contains(&RegisterTrait::HardwiredZero));
        // Both classes report the same shared register file.
        let classes: std::collections::HashMap<String, &crate::ast::RegisterClass> = files[0]
            .register_classes()
            .map(|c| (c.name.clone(), c))
            .collect();
        assert_eq!(gpr.register_file(&classes), "GPR");
        assert_eq!(gprsp.register_file(&classes), "GPR");
    }

    #[test]
    fn smoke_isa() {
        let code = "isa RV32I {}";
        let (tokens, mut _errors) = lexer().parse(code).into_output_errors();

        let tokens = tokens.unwrap();
        let isa = isa_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );

        println!("{:?}", isa);
        assert!(isa.has_output());
    }

    #[test]
    fn inline_expr_parses_less_equal() {
        let code = "a <= b";
        let (tokens, mut _errors) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = inline_expr().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let expr = parsed.output().unwrap().0.clone();
        match expr {
            Expr::Binary(bin) => assert_eq!(bin.op, BinOp::LessThenEqual),
            _ => panic!("Expected binary expression"),
        }
    }

    fn parse_expr(code: &str) -> Expr {
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        expr()
            .then(end())
            .parse(
                tokens
                    .as_slice()
                    .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
            )
            .output()
            .unwrap()
            .0
            .clone()
    }

    #[test]
    fn expr_parses_for_loop_as_accumulator() {
        let parsed = parse_expr("for i in 0..4 { rd = rd + i; }");
        let Expr::For(for_) = &parsed else {
            panic!("expected for loop, got {parsed:?}");
        };
        assert_eq!(for_.var, "i");
        // A single accumulating assignment is recognized as the fold form.
        assert!(for_.accumulator().is_some());
    }

    #[test]
    fn accumulator_loop_lowers_to_loop_node() {
        use tir::graph::Dag;
        use tir::sem_expr::{ExprKind, ExprPostGraph};

        // Symbolic bound `n`: the loop survives as a first-class `Loop` node
        // rather than being unrolled.
        let parsed = parse_expr("for i in 0..n { acc = acc + i }");
        let mut graph = ExprPostGraph::new();
        let lowering = parsed
            .lower_to_sema(&mut graph, &std::collections::HashMap::new())
            .expect("loop should lower");
        assert_eq!(*graph.get_node(lowering.root), ExprKind::Loop);
        let has_indvar = (0..graph.len())
            .any(|i| *graph.get_node(tir::graph::NodeId::from_index(i)) == ExprKind::IndVar);
        let has_acc = (0..graph.len())
            .any(|i| *graph.get_node(tir::graph::NodeId::from_index(i)) == ExprKind::Acc);
        assert!(
            has_indvar && has_acc,
            "loop body must reference IndVar and Acc"
        );
    }

    #[test]
    fn non_accumulator_loop_unrolls() {
        // A body that is not a single accumulating assignment falls back to
        // compile-time unrolling.
        let parsed = parse_expr("for i in 0..3 { store(i, 4, i); }");
        let Expr::Block(block) = parsed.expand_loops(&std::collections::HashMap::new()) else {
            panic!("unrolled loop must be a block");
        };
        assert_eq!(block.stmts.len(), 3);
    }

    #[test]
    fn inline_expr_parses_not_equal() {
        let code = "a != b";
        let (tokens, mut _errors) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = inline_expr().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let expr = parsed.output().unwrap().0.clone();
        match expr {
            Expr::Binary(bin) => assert_eq!(bin.op, BinOp::NotEqual),
            _ => panic!("Expected binary expression"),
        }
    }

    #[test]
    fn inline_expr_parses_bitwise_not() {
        let code = "a & ~1";
        let (tokens, mut _errors) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = inline_expr().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let expr = parsed.output().unwrap().0.clone();
        match expr {
            Expr::Binary(bin) => {
                assert_eq!(bin.op, BinOp::BitwiseAnd);
                match *bin.rhs {
                    Expr::Unary(un) => assert_eq!(un.op, UnOp::BitwiseNot),
                    _ => panic!("Expected unary expression"),
                }
            }
            _ => panic!("Expected binary expression"),
        }
    }

    #[test]
    fn smoke_unit_decl() {
        let code = "sched_class WriteIMul { latency = 3; throughput = 1; }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = unit_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let u = parsed.output().expect("unit decl should parse").0.clone();
        assert_eq!(u.name, "WriteIMul");
        assert_eq!(u.default_latency, Some(3));
        assert_eq!(u.default_throughput, Some(1));
    }

    #[test]
    fn smoke_unit_decl_bare() {
        let code = "sched_class WriteLoad;";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = unit_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let u = parsed
            .output()
            .expect("bare unit decl should parse")
            .0
            .clone();
        assert_eq!(u.name, "WriteLoad");
        assert_eq!(u.default_latency, None);
    }

    #[test]
    fn smoke_machine() {
        let code = "machine RocketCore for [RV64I] {
            issue_width = 2;
            buffers { rob = 64; lsq = 16; }
            reg_file { GPR { count = 96; } }
            unit ALU { count = 2; }
            unit MUL { count = 1; }
            bind WriteIALU { latency = 1; uses = [ALU]; }
            bind WriteIMul { latency = 3; uses = [MUL]; }
        }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = machine_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let m = parsed.output().expect("machine should parse").0.clone();
        assert_eq!(m.name, "RocketCore");
        assert_eq!(m.alias, None);
        assert_eq!(m.for_isas, vec!["RV64I".to_string()]);
        assert_eq!(m.issue_width, Some(2));
        assert_eq!(m.buffers, vec![("rob".into(), 64), ("lsq".into(), 16)]);
        assert_eq!(m.resources.len(), 2);
        assert_eq!(m.resources[0].name, "ALU");
        assert_eq!(m.resources[0].units, 2);
        assert_eq!(m.reg_files, vec![("GPR".into(), 96)]);
        assert_eq!(m.binds.len(), 2);
        assert_eq!(m.binds[0].unit, "WriteIALU");
        assert_eq!(m.binds[0].latency, Some(1));
        assert_eq!(m.binds[0].uses, vec!["ALU".to_string()]);
    }

    #[test]
    fn smoke_machine_alias() {
        let code = "machine InOrderCore (\"in-order\") for [RV64I] {
            issue_width = 1;
        }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = machine_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let m = parsed.output().expect("machine should parse").0.clone();
        assert_eq!(m.name, "InOrderCore");
        assert_eq!(m.alias.as_deref(), Some("in-order"));
        assert_eq!(m.for_isas, vec!["RV64I".to_string()]);
    }

    #[test]
    fn smoke_machine_pipeline_and_phase_bind() {
        use crate::ast::Protection;
        let code = "machine InOrder for [RV64I] {
            pipeline { IF; ID; EX: unprotected; MEM; WB; }
            unit LSU { count = 1; }
            bind WriteLoad { reads = ID; writes = MEM; uses = [LSU]; }
        }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = machine_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let m = parsed.output().expect("machine should parse").0.clone();
        assert_eq!(m.pipeline.len(), 5);
        assert_eq!(m.pipeline[0].name, "IF");
        assert_eq!(m.pipeline[0].protection, Protection::Protected);
        assert_eq!(m.pipeline[2].name, "EX");
        assert_eq!(m.pipeline[2].protection, Protection::Unprotected);
        assert_eq!(m.binds[0].reads.as_deref(), Some("ID"));
        assert_eq!(m.binds[0].writes.as_deref(), Some("MEM"));
    }

    #[test]
    fn smoke_machine_override_and_forward() {
        let code = "machine M for [RV64I] {
            unit ALU { count = 2; }
            unit LSU { count = 1; }
            bind WriteIALU { latency = 1; uses = [ALU]; }
            override Mul { latency = 3; uses = [ALU]; }
            forward ALU => ALU { latency = 0; }
            forward LSU => ALU { latency = 1; }
        }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = machine_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let m = parsed.output().expect("machine should parse").0.clone();
        assert_eq!(m.overrides.len(), 1);
        assert_eq!(m.overrides[0].instruction, "Mul");
        assert_eq!(m.overrides[0].latency, Some(3));
        assert_eq!(m.overrides[0].uses, vec!["ALU".to_string()]);
        assert_eq!(m.forwards.len(), 2);
        assert_eq!(
            (
                m.forwards[0].from.as_str(),
                m.forwards[0].to.as_str(),
                m.forwards[0].latency
            ),
            ("ALU", "ALU", 0)
        );
        assert_eq!(
            (
                m.forwards[1].from.as_str(),
                m.forwards[1].to.as_str(),
                m.forwards[1].latency
            ),
            ("LSU", "ALU", 1)
        );
    }

    #[test]
    fn smoke_instruction_schedule() {
        let code = "instruction Mul : MulDivOp {
            behavior { rd = rs1; }
            schedule { units = [WriteIMul]; }
        }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = instruction_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let inst = parsed.output().expect("instruction should parse").0.clone();
        let sched = inst.schedule.expect("schedule block should be present");
        assert_eq!(sched.classes, vec!["WriteIMul".to_string()]);
    }

    #[test]
    fn instruction_without_schedule_is_none() {
        let code = "instruction Add : ALUOp { behavior { rd = rs1; } }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = instruction_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let inst = parsed.output().expect("instruction should parse").0.clone();
        assert!(inst.schedule.is_none());
    }

    #[test]
    fn operand_width_expression_parses() {
        let code = "instruction Foo : Bar {
            operands { a: bits<6>, b: bits<log2Ceil(self.XLEN)>, }
            behavior { rd = rs1; }
        }";
        let (tokens, _e) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = instruction_def().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let inst = parsed.output().expect("instruction should parse").0.clone();
        assert_eq!(inst.operands[0].1, crate::Type::Bits(6));
        assert!(matches!(inst.operands[1].1, crate::Type::BitsExpr(_)));
    }

    #[test]
    fn inline_expr_parses_unsigned_less_equal() {
        let code = "a ~<= b";
        let (tokens, mut _errors) = lexer().parse(code).into_output_errors();
        let tokens = tokens.unwrap();
        let parsed = inline_expr().then(end()).parse(
            tokens
                .as_slice()
                .map((code.len()..code.len()).into(), |(t, s)| (t, s)),
        );
        let expr = parsed.output().unwrap().0.clone();
        match expr {
            Expr::Binary(bin) => assert_eq!(bin.op, BinOp::UnsignedLessThenEqual),
            _ => panic!("Expected binary expression"),
        }
    }
}
