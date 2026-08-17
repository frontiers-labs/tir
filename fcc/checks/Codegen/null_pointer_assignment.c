// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s --check-prefix=IR
// RUN: fcc compile --march x86_64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct node {
  struct node *next;
};

void clear(struct node *node)
{
  node->next = (struct node *)0;
}

// IR-LABEL: func.func @clear
// IR: %[[ZERO:.*]] = ptr.null : !ptr.p
// IR: ptr.store %[[ZERO]]

// ASM-LABEL: clear:
// ASM: mov [{{.*}}], {{r[a-z0-9]+}}
