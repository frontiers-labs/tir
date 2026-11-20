use tir_egglog::{EGraph, Error};

pub(super) fn install(engine: &mut EGraph) -> Result<(), Error> {
    let absent = engine.base_to_value(-1_i64);
    let db = engine.database_mut();
    for name in ["Operation", "Region", "Block", "Value", "Control"] {
        db.add_unique_key(db.table(name)?, &[0], None)?;
    }
    db.add_unique_key(db.table("Region")?, &[2, 3], Some((2, absent)))?;
    db.add_unique_key(db.table("Value")?, &[3, 2, 4], Some((3, absent)))?;
    Ok(())
}
