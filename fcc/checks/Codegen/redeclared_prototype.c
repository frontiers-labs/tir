// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

// Repeating a prototype declares the same symbol again, not an overload of it,
// so the module names it once.

void abort(void);
extern void abort(void);

int main(void) {
  abort();
  return 0;
}

// CHECK-COUNT-1: %{{[0-9]+}} = func.declare @abort() -> !unit
// CHECK-NOT: %{{[0-9]+}} = func.declare @abort
