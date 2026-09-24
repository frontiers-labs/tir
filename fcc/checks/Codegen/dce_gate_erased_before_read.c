// RUN: fcc compile -O2 --stage obj --march x86_64 -o %t.o %s
// RUN: fcc compile -O2 --stage obj --march x86_64 --shuffle-machine-order -o %t.o %s
// RUN: fcc compile -O2 --stage obj --march x86_64 --pipeline func.func(promote-nodes),fixpoint<3>(func.func(verify-deps,instcombine-nodes,affine,instcombine-nodes)) -o %t.o %s

// Nightly differential-fuzz #498 #499 #500. The gate deciding `b` is dead
// once `loc` is, and dce erases it with its regions. The load of `q[b & 3]`
// dies after, and renaming the state it published over every region dce
// had listed up front hit the erased ones and fcc panicked on `live region`.

int f0(int *p, int *q, int a, int b)
{
    int loc[8] = {-736, -671, -709, 568, -553, -601, -155, 964};
    if (((-45 | (-49 + 42)) <= p[(a) & 3])) {
        b = (-22 % (8));
    }
    *(loc + ((b) & 7)) = q[(b) & 3];
    return (-18 & -34);
}
