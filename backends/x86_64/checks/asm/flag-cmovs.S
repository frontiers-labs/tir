# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s --check-prefix=IR
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC
# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefix=EXEC

.global done
done:
    ret
.global _start
_start:
    mov rbx, 11
    mov r8, 22
    mov rdx, 33
    mov r9, 44
    mov rax, -1
    test rax, rax
    cmovs rbx, rdx
    mov rax, 1
    test rax, rax
    cmovs r8, r9

# IR: x86_64.cmovs {dst = rbx, src = rdx}
# IR: x86_64.cmovs {dst = r8, src = r9}
# ENC: [0x48, 0x0F, 0x48, 0xDA]
# ENC: [0x4D, 0x0F, 0x48, 0xC1]
# EXEC: final registers:
# EXEC-DAG: GPR[3] = 0x21
# EXEC-DAG: GPR[8] = 0x16
# EXEC-DAG: GPR[2] = 0x21
# EXEC-DAG: GPR[9] = 0x2c
