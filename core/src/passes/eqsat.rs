mod publish;

use tir_egglog::{Color, Database, Program, Row, Schedule, extract::Extractor, sort::BigInt};

use crate::{AnalysisManager, Context, OpId, Pass, PassError, RegionId, ValueId};

/// Saturate integer equalities in a private snapshot of the current IR.
#[derive(Default)]
pub struct EqSatPass;

#[linkme::distributed_slice(crate::PASSES)]
static EQSAT: crate::PassInfo = crate::PassInfo {
    name: "eqsat",
    ctor: parse_eqsat,
};

fn parse_eqsat(args: &str) -> Result<Box<dyn Pass>, String> {
    if args.is_empty() {
        Ok(Box::new(EqSatPass))
    } else {
        Err("pass 'eqsat' takes no arguments".into())
    }
}

fn rule_error(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(error.to_string())
}

impl Pass for EqSatPass {
    fn name(&self) -> &'static str {
        "eqsat"
    }

    fn run(
        &mut self,
        root_id: OpId,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let (mut db, mut program) = snapshot(context, root_id)?;
        let analysis = Schedule::seq([
            Schedule::run("eqsat-flag-fields"),
            Schedule::run("flag_values"),
            Schedule::repeat(4, "memory_barriers"),
            Schedule::run("gamma_witness"),
            Schedule::run("gamma_missing"),
            Schedule::run("gamma_agrees"),
            Schedule::repeat(64, "eqsat-global"),
            Schedule::repeat(4, "incoming"),
            Schedule::run("phi_candidates"),
            Schedule::run("phi_disagreement"),
            Schedule::run("phi_literals"),
            Schedule::run("memory_cfg"),
            Schedule::run("memory_cfg_missing"),
            Schedule::run("memory_cfg_apply"),
            Schedule::run("memory_join_missing"),
            Schedule::run("memory_join_apply"),
            Schedule::repeat(64, "eqsat-global"),
            Schedule::repeat(4, "removable"),
            Schedule::repeat(8, "plain_access"),
            Schedule::run("plain_disjoint"),
            Schedule::run("private_call"),
            Schedule::run("plain_clobber"),
            Schedule::run("safe_control_inputs"),
            Schedule::run("control_effects"),
            Schedule::repeat(2, "finite_loops"),
            Schedule::repeat(4, "switch_cases"),
            Schedule::run("switch_default"),
            Schedule::run("pure_switch"),
            Schedule::run("switch_values"),
            Schedule::run("switch_agreement"),
            Schedule::run("switch_aliases"),
            Schedule::run("plain_forward"),
            Schedule::run("dead_store_proof"),
            Schedule::run("dead_store_uses"),
            Schedule::run("dead_store"),
            Schedule::repeat(64, "paths"),
            Schedule::repeat(4, "counted_exits"),
            Schedule::run("counted_missing"),
            Schedule::run("counted_agrees"),
            Schedule::run("counted_reads"),
            Schedule::repeat(64, "paths"),
            Schedule::run("used"),
            Schedule::repeat(4, "edits"),
            Schedule::run("phi_uses"),
            Schedule::run("definition_constants"),
            Schedule::run("definition_expressions"),
        ]);
        program.run(&mut db, &analysis, &[]).map_err(rule_error)?;
        program
            .run(
                &mut db,
                &Schedule::seq([
                    Schedule::run("incoming"),
                    Schedule::repeat(64, "assumptions"),
                ]),
                &[],
            )
            .map_err(rule_error)?;
        let local = scoped_literals(&mut db, &mut program)?;
        let table = |name| db.table(name).map_err(rule_error);
        let forms = table("EqSatDefinition")?;
        let mut rebuild = Vec::new();
        if db.row_count(forms) != 0 {
            let extractor = Extractor::new(&db);
            for row in db.rows(forms) {
                let (terms, term) = extractor.best(row[2]).map_err(rule_error)?;
                rebuild.push((
                    OpId::from_number(entity(&db, row, 0)?),
                    ValueId::from_number(entity(&db, row, 1)?),
                    terms,
                    term,
                ));
            }
        }
        let mut edits = Vec::new();
        for row in db.rows(table("EqSatAliasEdit")?) {
            let site = use_site(&db, row, db.base_value(row[4]))?;
            let old = ValueId::from_number(entity(&db, row, 2)?);
            let new = ValueId::from_number(entity(&db, row, 3)?);
            edits.push((site, old, new));
        }
        let mut redirected = db
            .rows(table("EqSatRedirect")?)
            .map(|row| entity(&db, row, 0).map(ValueId::from_number))
            .collect::<Result<Vec<_>, _>>()?;
        let mut erase = db
            .rows(table("EqSatErase")?)
            .map(|row| entity(&db, row, 0).map(OpId::from_number))
            .collect::<Result<Vec<_>, _>>()?;
        redirected.sort_unstable();
        erase.sort_unstable();
        publish::publish(context, rebuild, edits, local, &redirected, &erase)?;
        if context.has_operation(root_id) {
            sweep(context, root_id)?;
        }
        Ok(())
    }
}

fn snapshot(context: &Context, root: OpId) -> Result<(Database, Program), PassError> {
    let mut db = context.eqsat_database().map_err(rule_error)?;
    let mut program = Program::from_bytes(
        include_bytes!(concat!(env!("OUT_DIR"), "/eqsat-rules.bin")),
        &mut db,
    )
    .map_err(rule_error)?;
    let table = db.table("EqSatRoot").map_err(rule_error)?;
    let root = db.base(i64::from(root.raw()));
    db.insert(table, &[root]).map_err(rule_error)?;
    program
        .run(
            &mut db,
            &Schedule::seq([
                Schedule::saturate("select"),
                Schedule::repeat(8, "frame"),
                Schedule::run("sites"),
                Schedule::repeat(4, "layout_queries"),
            ]),
            &[],
        )
        .map_err(rule_error)?;
    context.eqsat_layout_values(&mut db).map_err(rule_error)?;
    Ok((db, program))
}

fn sweep(context: &Context, root: OpId) -> Result<(), PassError> {
    let (mut db, mut program) = snapshot(context, root)?;
    program
        .run(
            &mut db,
            &Schedule::seq([
                Schedule::run("congruence"),
                Schedule::repeat(4, "scalars"),
                Schedule::repeat(4, "cast_cse"),
                Schedule::repeat(8, "removable"),
                Schedule::repeat(2, "integer_inputs"),
                Schedule::run("eqsat-fold"),
                Schedule::saturate("address_keys"),
                Schedule::repeat(8, "plain_access"),
                Schedule::saturate("storage_defined"),
                Schedule::run("private_layout"),
                Schedule::run("private_extent"),
                Schedule::repeat(8, "private_store"),
                Schedule::run("private_other"),
                Schedule::run("private_drop"),
                Schedule::run("plain_overwrites"),
                Schedule::run("overwrite_barriers"),
                Schedule::run("overwrite_plain"),
                Schedule::run("safe_control_inputs"),
                Schedule::run("control_effects"),
                Schedule::run("pure_switch"),
                Schedule::saturate("demand"),
                Schedule::run("dead"),
            ]),
            &[],
        )
        .map_err(rule_error)?;
    let table = db.table("EqSatDead").map_err(rule_error)?;
    let removals = db
        .rows(table)
        .map(|row| {
            let id = OpId::from_number(entity(&db, row, 0)?);
            Ok(context.get_op(id))
        })
        .collect::<Result<Vec<_>, PassError>>()?;
    publish::erase_operations(context, removals)
}

fn scoped_literals(
    db: &mut Database,
    program: &mut Program,
) -> Result<Vec<publish::Local>, PassError> {
    let seed = db.table("EqSatColorSeed").map_err(rule_error)?;
    let site = db.table("EqSatColorSite").map_err(rule_error)?;
    let subject = db.table("EqSatColorSubject").map_err(rule_error)?;
    let mut seeds = db
        .rows(seed)
        .map(|row| [row[0], row[1], row[2]])
        .collect::<Vec<_>>();
    seeds.sort_unstable_by_key(|row| row[0]);
    let mut colors = Vec::new();
    for group in seeds.chunk_by(|a, b| a[0] == b[0]) {
        let Some(&[scope, _, _]) = group.first() else {
            continue;
        };
        let color = db.color(&[Color::BASE]);
        colors.push(color);
        db.insert_in(color, site, &[scope]).map_err(rule_error)?;
        for &[_, old, literal] in group {
            db.insert_in(color, subject, &[old]).map_err(rule_error)?;
            db.union_in(color, old, literal);
        }
    }
    if !colors.is_empty() {
        db.rebuild().map_err(rule_error)?;
        program
            .run(db, &Schedule::repeat(8, "branch_local"), &[])
            .map_err(rule_error)?;
    }
    let mut edits = Vec::new();
    let table = db.table("EqSatScopedEdit").map_err(rule_error)?;
    for row in std::iter::once(Color::BASE)
        .chain(colors)
        .flat_map(|color| db.rows_in(color, table))
    {
        let site = use_site(db, row, db.base_value(row[4]))?;
        let old = ValueId::from_number(entity(db, row, 2)?);
        edits.push((site, old, db.base_value::<BigInt>(row[3])));
    }
    Ok(edits)
}

fn use_site(db: &Database, row: Row<'_>, is_yield: bool) -> Result<publish::UseSite, PassError> {
    let owner = entity(db, row, 0)?;
    let slot = entity(db, row, 1)? as usize;
    Ok(if is_yield {
        publish::UseSite::Yield(RegionId::from_number(owner), slot)
    } else {
        publish::UseSite::Operand(OpId::from_number(owner), slot)
    })
}

fn entity(db: &Database, row: Row<'_>, slot: usize) -> Result<u32, PassError> {
    u32::try_from(db.base_value::<i64>(row[slot])).map_err(rule_error)
}
