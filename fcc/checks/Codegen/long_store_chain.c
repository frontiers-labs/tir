// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: fcc compile --march x86_64 --stage asm -o - %s | filecheck %s
// RUN: fcc compile -O2 --march x86_64 --stage asm -o - %s | filecheck %s

// A statement reads the state its predecessor's store left, so a tree cost
// counts that state under the load and under the store and doubles from one
// statement to the next. Sixty statements pass the largest price there is,
// and every one of them still has a spelling.
void store_chain(long *p, long *q, long i)
{
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
  p[i] = q[i] + 1;
}

// CHECK-LABEL: store_chain:
// CHECK-COUNT-60: mov [{{.*}}], r
// CHECK: ret
