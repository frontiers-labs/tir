// RUN: fcc cc -### -S %s -o out.s | filecheck %s --check-prefix=ASM
// RUN: fcc cc -### -E %s -o - | filecheck %s --check-prefix=PRE

// Each stop flag carries its phase into the planned compile action.

// ASM: "fcc" "-S" "-o" "out.s" "{{.*}}stop_phases.c"
// PRE: "fcc" "-E" "-o" "-" "{{.*}}stop_phases.c"
