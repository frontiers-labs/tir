# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s --check-prefix=IR
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC
# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefix=EXEC

.global done
done:
    ret
.global _start
_start:
    mov rsi, -1
    mov r9, -1
    mov rax, -1
    test rax, rax
    sets sil
    mov rax, 1
    test rax, rax
    sets r9b

# IR: x86_64.sets {dst = sil}
# IR: x86_64.sets {dst = r9b}
# ENC: [0x40, 0x0F, 0x98, 0xC6]
# ENC: [0x41, 0x0F, 0x98, 0xC1]
# EXEC: final registers:
# EXEC-DAG: GPR[6] = 0xffffffffffffff01
# EXEC-DAG: GPR[9] = 0xffffffffffffff00
