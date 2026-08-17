//! End-to-end JIT tests: compile TIR IR and call the result on the host.
//! Gated to the architectures the JIT can currently target.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::ffi::c_void;

use tir_jit::Jit;

#[test]
fn add_two_integers() {
    let ir = r#"
        module {
          func.func @add(%0: !i64, %1: !i64) -> !i64 {
            %2 = addi %0, %1 : !i64
            func.return %2
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
          func.func @chain(%0: !i64, %1: !i64) -> !i64 {
            %2 = addi %0, %1 : !i64
            %3 = subi %0, %2 : !i64
            %4 = muli %3, %1 : !i64
            func.return %4
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
          func.func @scale(%0: !i64) -> !i64 {
            %1 = constant {value = 7} : !i64
            %2 = muli %0, %1 : !i64
            %3 = constant {value = -3} : !i64
            %4 = muli %2, %3 : !i64
            func.return %4
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
          func.func @mul(%0: !i64, %1: !i64) -> !i64 {
            %2 = muli %0, %1 : !i64
            func.return %2
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
          func.func @lt(%0: !i64, %1: !i64) -> !i64 {
            %2 = cmpi %0, %1 {predicate = "slt"} : !i1
            cfg.cond_br %2, ^bb1, ^bb2
          ^bb1:
            %3 = constant {value = 1} : !i64
            func.return %3
          ^bb2:
            %4 = constant {value = 0} : !i64
            func.return %4
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

#[test]
fn value_live_across_branch() {
    // %v is computed in the entry block but only used in ^bb1, so it is live
    // across the conditional branch. Register allocation must keep it intact
    // through the branch and the ^bb1 temporary; a miscompile here (from empty
    // CFG successors in liveness) returns a clobbered value.
    let ir = r#"
        module {
          func.func @cross(%0: !i64, %1: !i64) -> !i64 {
            %v = addi %0, %1 : !i64
            %c = cmpi %0, %1 {predicate = "slt"} : !i1
            cfg.cond_br %c, ^bb1, ^bb2
          ^bb1:
            %t = addi %0, %0 : !i64
            %r = addi %v, %t : !i64
            func.return %r
          ^bb2:
            %s = addi %1, %1 : !i64
            func.return %s
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let cross: extern "C" fn(i64, i64) -> i64 =
        unsafe { module.get("cross") }.expect("cross symbol");
    // a < b  => (a + b) + 2a = 3a + b ; else => 2b
    assert_eq!(cross(3, 10), 19);
    assert_eq!(cross(20, 4), 8);
    assert_eq!(cross(-8, -1), -25);
}

#[test]
fn returns_first_argument_directly() {
    // f(a, b) = a returns its first argument directly. The argument register
    // (rdi on SysV x86-64) differs from the return register (rax), so pinning the
    // same vreg to both silently overwrote the argument pin and the function
    // returned whatever happened to be in rax. The fix breaks the point conflict
    // with a copy into the return register.
    let ir = r#"
        module {
          func.func @first(%0: !i64, %1: !i64) -> !i64 {
            func.return %0
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { module.get("first") }.expect("first symbol");
    assert_eq!(f(42, 7), 42);
    assert_eq!(f(-3, 100), -3);
}

#[test]
fn block_argument_diamond() {
    // A diamond whose two arms forward different values (%a, %b) into the merge
    // block's parameter %r on unconditional edges. Register allocation lowers each
    // forwarded value to an explicit copy into %r's register, so the merge sees a
    // single consistently-colored parameter regardless of the path taken.
    let ir = r#"
        module {
          func.func @sel(%c: !i64, %d: !i64, %a: !i64, %b: !i64) -> !i64 {
            %cond = cmpi %c, %d {predicate = "slt"} : !i1
            cfg.cond_br %cond, ^bb1, ^bb2
          ^bb1:
            cfg.br ^bb3(%a : !i64)
          ^bb2:
            cfg.br ^bb3(%b : !i64)
          ^bb3(%r: !i64):
            func.return %r
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let sel: extern "C" fn(i64, i64, i64, i64) -> i64 =
        unsafe { module.get("sel") }.expect("sel symbol");
    // sel(c, d, a, b) = if c < d { a } else { b }
    assert_eq!(sel(1, 2, 42, 7), 42);
    assert_eq!(sel(5, 2, 42, 7), 7);
    assert_eq!(sel(-9, 0, 100, 200), 100);
}

#[test]
fn loop_carried_block_argument() {
    let ir = r#"
        module {
          func.func @count(%limit: !i64) -> !i64 {
            %zero = constant {value = 0} : !i64
            cfg.br ^bb1(%zero : !i64)
          ^bb1(%iv: !i64):
            %keep_going = cmpi %iv, %limit {predicate = "slt"} : !i1
            cfg.cond_br %keep_going, ^bb2, ^bb3
          ^bb2:
            %one = constant {value = 1} : !i64
            %next = addi %iv, %one : !i64
            cfg.br ^bb1(%next : !i64)
          ^bb3:
            func.return %iv
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let count: extern "C" fn(i64) -> i64 = unsafe { module.get("count") }.expect("count symbol");
    assert_eq!(count(0), 0);
    assert_eq!(count(5), 5);
    assert_eq!(count(23), 23);
}

#[test]
fn loop_accumulator_in_a_slot() {
    // The accumulator lives in memory and is mutated every iteration, so the
    // result pins what the mid-end does to a slot a loop carries.
    let ir = r#"
        module {
          func.func @sum(%limit: !i64) -> !i64 {
            %acc = ptr.alloca {size = 8, align = 8} : !ptr.p<!i64>
            %zero = constant {value = 0} : !i64
            ptr.store %zero, %acc
            cfg.br ^bb1(%zero : !i64)
          ^bb1(%iv: !i64):
            %keep_going = cmpi %iv, %limit {predicate = "slt"} : !i1
            cfg.cond_br %keep_going, ^bb2, ^bb3
          ^bb2:
            %current = ptr.load %acc : !i64
            %grown = addi %current, %iv : !i64
            ptr.store %grown, %acc
            %one = constant {value = 1} : !i64
            %next = addi %iv, %one : !i64
            cfg.br ^bb1(%next : !i64)
          ^bb3:
            %total = ptr.load %acc : !i64
            func.return %total
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let sum: extern "C" fn(i64) -> i64 = unsafe { module.get("sum") }.expect("sum symbol");
    // sum(n) = 0 + 1 + ... + (n - 1)
    assert_eq!(sum(0), 0);
    assert_eq!(sum(5), 10);
    assert_eq!(sum(23), 253);
}

#[test]
fn conditional_edge_arguments() {
    // The arms' forwarded values ride the conditional branch's own edges. The
    // true edge is split into a trampoline block during selection; the false
    // edge's value rides the fallthrough branch. Register allocation lowers each
    // to a copy into the merge parameter's register.
    let ir = r#"
        module {
          func.func @sel(%c: !i64, %d: !i64, %a: !i64, %b: !i64) -> !i64 {
            %cond = cmpi %c, %d {predicate = "slt"} : !i1
            cfg.cond_br %cond, ^bb1(%a : !i64), ^bb2(%b : !i64)
          ^bb1(%x: !i64):
            func.return %x
          ^bb2(%y: !i64):
            func.return %y
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target");
    let module = jit.compile(ir).expect("compile");
    let sel: extern "C" fn(i64, i64, i64, i64) -> i64 =
        unsafe { module.get("sel") }.expect("sel symbol");
    // sel(c, d, a, b) = if c < d { a } else { b }
    assert_eq!(sel(1, 2, 42, 7), 42);
    assert_eq!(sel(5, 2, 42, 7), 7);
    assert_eq!(sel(-9, 0, 100, 200), 100);
}

extern "C" fn host_triple(x: i64) -> i64 {
    x * 3
}

#[test]
fn external_host_call() {
    // The call is the tail expression: nothing is live across it.
    let ir = r#"
        module {
          func.declare @host_triple(!i64) -> !i64
          func.func @via_host(%0: !i64) -> !i64 {
            %1 = func.call @host_triple(%0 : !i64) -> !i64
            func.return %1
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

#[test]
fn value_live_across_host_call() {
    // %a is computed, passed to host_triple, and read again after the call, so it
    // is live across it and must occupy a callee-saved register. The function must
    // preserve that register for its own caller: the prologue/epilogue now save and
    // restore the assigned callee-saved registers (and, on riscv/arm64, the return
    // address saved across the call rides the same mechanism).
    let ir = r#"
        module {
          func.declare @host_triple(!i64) -> !i64
          func.func @f(%0: !i64) -> !i64 {
            %a = addi %0, %0 : !i64
            %1 = func.call @host_triple(%a : !i64) -> !i64
            %2 = addi %a, %1 : !i64
            func.return %2
          }
          module_end
        }
    "#;

    let mut jit = Jit::host().expect("host target");
    jit.define_symbol("host_triple", host_triple as *const c_void);
    let module = jit.compile(ir).expect("compile");
    let f: extern "C" fn(i64) -> i64 = unsafe { module.get("f") }.expect("f symbol");
    // f(x) = 2x + host_triple(2x) = 2x + 6x = 8x
    assert_eq!(f(5), 40);
    assert_eq!(f(-3), -24);
}

// Cross-compile to AArch64 and load (map + patch relocations + mprotect) without
// executing, exercising the AArch64 relocation and trampoline math on real
// generated code even when the host is x86-64.
#[test]
fn aarch64_cross_load() {
    let branch = r#"
        module {
          func.func @lt(%0: !i64, %1: !i64) -> !i64 {
            %2 = cmpi %0, %1 {predicate = "slt"} : !i1
            cfg.cond_br %2, ^bb1, ^bb2
          ^bb1:
            %3 = constant {value = 1} : !i64
            func.return %3
          ^bb2:
            %4 = constant {value = 0} : !i64
            func.return %4
          }
          module_end
        }
    "#;
    let jit = Jit::new("arm64", None);
    let module = jit.compile(branch).expect("cross-compile branch for arm64");
    assert!(module.address("lt").is_some());

    let external = r#"
        module {
          func.declare @host_triple(!i64) -> !i64
          func.func @via_host(%0: !i64) -> !i64 {
            %1 = func.call @host_triple(%0 : !i64) -> !i64
            func.return %1
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
