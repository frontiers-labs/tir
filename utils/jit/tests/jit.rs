//! End-to-end JIT tests: compile TIR IR and call the result on the host.
//! Gated to the architectures the JIT can currently target.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::ffi::c_void;

use tir_jit::Jit;

#[test]
fn add_two_integers() {
    let ir = r#"
        module {
          func @add(%0: !i64, %1: !i64) -> !i64 {
            %2 = addi %0, %1 : !i64
            return %2
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let add: extern "C" fn(i64, i64) -> i64 = unsafe { module.get("add") }.expect("add symbol");
    assert_eq!(add(2, 40), 42);
    assert_eq!(add(-5, 5), 0);
}

#[test]
fn arithmetic_chain() {
    let ir = r#"
        module {
          func @chain(%0: !i64, %1: !i64) -> !i64 {
            %2 = addi %0, %1 : !i64
            %3 = subi %0, %2 : !i64
            %4 = muli %3, %1 : !i64
            return %4
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { module.get("chain") }.expect("chain symbol");
    // chain(a,b) = (a - (a+b)) * b = (-b) * b
    assert_eq!(f(3, 4), -16);
    assert_eq!(f(10, 7), -49);
}

#[test]
fn multiply_by_constant() {
    // Exercises the immediate-multiply form (`imul r, r/m, imm`).
    let ir = r#"
        module {
          func @scale(%0: !i64) -> !i64 {
            %1 = constant {value = 7} : !i64
            %2 = muli %0, %1 : !i64
            %3 = constant {value = -3} : !i64
            %4 = muli %2, %3 : !i64
            return %4
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let scale: extern "C" fn(i64) -> i64 = unsafe { module.get("scale") }.expect("scale symbol");
    // scale(x) = (x * 7) * -3 = -21x
    assert_eq!(scale(2), -42);
    assert_eq!(scale(-5), 105);
}

#[test]
fn multiply_registers() {
    let ir = r#"
        module {
          func @mul(%0: !i64, %1: !i64) -> !i64 {
            %2 = muli %0, %1 : !i64
            return %2
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let mul: extern "C" fn(i64, i64) -> i64 = unsafe { module.get("mul") }.expect("mul symbol");
    assert_eq!(mul(6, 7), 42);
    assert_eq!(mul(-4, 8), -32);
}

#[test]
fn conditional_branch() {
    // Local branches resolve as pc-relative block fixups (no relocations):
    // returns 1 when a < b, else 0.
    let ir = r#"
        module {
          func @lt(%0: !i64, %1: !i64) -> !i64 {
            %2 = cmpi %0, %1 {predicate = "slt"} : !i1
            cond_br %2, ^bb1, ^bb2
          ^bb1:
            %3 = constant {value = 1} : !i64
            return %3
          ^bb2:
            %4 = constant {value = 0} : !i64
            return %4
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let lt: extern "C" fn(i64, i64) -> i64 = unsafe { module.get("lt") }.expect("lt symbol");
    assert_eq!(lt(3, 9), 1);
    assert_eq!(lt(20, 4), 0);
    assert_eq!(lt(-8, -1), 1);
}

extern "C" fn host_triple(x: i64) -> i64 {
    x * 3
}

#[test]
fn external_host_call() {
    // A value live across a call would need a callee-saved register, which the
    // backend's regalloc does not yet support; the call is the tail expression.
    let ir = r#"
        module {
          declare @host_triple(!i64) -> !i64
          func @via_host(%0: !i64) -> !i64 {
            %1 = call @host_triple(%0 : !i64) -> !i64
            return %1
          }
          module_end
        }
    "#;

    let mut jit = Jit::host().expect("host target");
    jit.define_symbol("host_triple", host_triple as *const c_void);
    let module = jit.compile(ir).expect("compile");
    let f: extern "C" fn(i64) -> i64 = unsafe { module.get("via_host") }.expect("via_host symbol");
    // via_host(x) = host_triple(x) = 3x
    assert_eq!(f(5), 15);
    assert_eq!(f(-3), -9);
}

// Cross-compile to AArch64 and load (map + patch relocations + mprotect) without
// executing, exercising the AArch64 relocation and trampoline math on real
// generated code even when the host is x86-64.
#[test]
fn aarch64_cross_load() {
    let branch = r#"
        module {
          func @lt(%0: !i64, %1: !i64) -> !i64 {
            %2 = cmpi %0, %1 {predicate = "slt"} : !i1
            cond_br %2, ^bb1, ^bb2
          ^bb1:
            %3 = constant {value = 1} : !i64
            return %3
          ^bb2:
            %4 = constant {value = 0} : !i64
            return %4
          }
          module_end
        }
    "#;
    let jit = Jit::new("arm64", None);
    let module = jit.compile(branch).expect("cross-compile branch for arm64");
    assert!(module.address("lt").is_some());

    let external = r#"
        module {
          declare @host_triple(!i64) -> !i64
          func @via_host(%0: !i64) -> !i64 {
            %1 = call @host_triple(%0 : !i64) -> !i64
            return %1
          }
          module_end
        }
    "#;
    let mut jit = Jit::new("arm64", None);
    jit.define_symbol("host_triple", host_triple as *const c_void);
    let module = jit
        .compile(external)
        .expect("cross-compile external call for arm64 (trampoline path)");
    assert!(module.address("via_host").is_some());
}
