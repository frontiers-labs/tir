// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

// `static` is internal linkage, which is `sym_visibility = private` on the λ.
// C attaches it to the entity, not to one declaration of it, so a `static`
// prototype makes the definition that follows it private too.

static int hidden(int x)
{
    return x + 1;
}

static int declared_first(int x);

int declared_first(int x)
{
    return x + 2;
}

int exposed(int x)
{
    return hidden(x) + declared_first(x);
}

// CHECK: func.func private @hidden
// CHECK: func.func private @declared_first
// CHECK: func.func @exposed
// CHECK-NOT: func.func private @exposed
