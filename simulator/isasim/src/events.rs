use serde_json::{Value, json};
use tir::backend::exec::{MemoryEffect, ResponseValue};
use tir_sim::Executor;

pub fn write(executor: &Executor, path: &str, engine: &str, success: bool) {
    let events: Vec<_> = executor.effect_events().iter().map(|event| {
        let request = &event.request;
        let mut value = match &request.effect {
            MemoryEffect::Read { address, size } => json!({"kind": "read", "address": address, "size": size}),
            MemoryEffect::Write { address, bytes } => json!({"kind": "write", "address": address, "bytes": bytes}),
            MemoryEffect::LoadReserved { address, size, ordering } => json!({"kind": "load_reserved", "address": address, "size": size, "ordering": format!("{ordering:?}")}),
            MemoryEffect::StoreConditional { address, size, value, ordering } => json!({"kind": "store_conditional", "address": address, "size": size, "value": value, "ordering": format!("{ordering:?}")}),
            MemoryEffect::AtomicRmw { op, address, size, value, ordering } => json!({"kind": "atomic_rmw", "operation": format!("{op:?}"), "address": address, "size": size, "value": value, "ordering": format!("{ordering:?}")}),
            MemoryEffect::Fence { pred, succ, kind } => json!({"kind": "fence", "pred": pred, "succ": succ, "fence_kind": kind}),
            MemoryEffect::Exception { cause } => json!({"kind": "exception", "cause": cause}),
        };
        value["instruction"] = json!(request.id.instruction);
        value["sequence"] = json!(request.id.sequence);
        value["pc"] = json!(request.pc);
        value["memory_order"] = json!(event.memory_order);
        value["response"] = match &event.result {
            Ok(ResponseValue::Word(word)) => json!(word),
            Ok(ResponseValue::Bytes(bits)) => json!(bits.bytes()),
            Ok(ResponseValue::Done) => Value::Null,
            Err(trap) => json!({"fault": trap_value(trap)}),
        };
        value
    }).collect();
    let fault = executor.last_fault().map(|fault| {
        json!({
            "pc": fault.pc,
            "sequence": fault.sequence,
        "cause": trap_value(&fault.trap),
        })
    });
    let report = json!({
        "schema_version": 1,
        "engine": engine,
        "address_space": format!("{:?}", executor.memory_service().address_space().id()),
        "status": if success { "complete" } else { "fault" },
        "events": events,
        "fault": fault,
    });
    let bytes = serde_json::to_vec_pretty(&report).expect("effect report is serializable");
    if path == "-" {
        use std::io::Write;
        std::io::stdout()
            .write_all(&bytes)
            .expect("failed to write effect report");
    } else {
        std::fs::write(path, bytes).expect("failed to write effect report");
    }
}

fn trap_value(trap: &tir::backend::SimTrap) -> Value {
    use tir::backend::SimTrap;
    match trap {
        SimTrap::MemoryFault {
            address,
            size,
            access,
            reason,
        } => json!({
            "kind": "memory", "address": address, "size": size, "access": access, "reason": reason,
        }),
        SimTrap::BadAddress { address, size } => {
            json!({"kind": "bad_address", "address": address, "size": size})
        }
        SimTrap::Exception { cause, pc } => json!({"kind": "exception", "cause": cause, "pc": pc}),
        SimTrap::PcNotMapped { pc } => {
            json!({"kind": "fetch", "pc": pc, "reason": "unmapped instruction"})
        }
        SimTrap::InvalidInstruction { op, reason } => {
            json!({"kind": "instruction", "op": op, "reason": reason})
        }
        other => json!({"kind": "execution", "reason": format!("{other:?}")}),
    }
}
