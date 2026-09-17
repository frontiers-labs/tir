# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s --check-prefix=IR
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC
# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefix=EXEC

.global done
done:
    ret
.global _start
_start:
    mov rbx, -1
    mov r8, -1
    movabs rax, 0x7fffffffffffffff
    add rax, 1
    setno bl
    test rax, rax
    setno r8b

# IR: x86_64.setno {dst = bl}
# IR: x86_64.setno {dst = r8b}
# ENC: [0x0F, 0x91, 0xC3]
# ENC: [0x41, 0x0F, 0x91, 0xC0]
# EXEC: final registers:
# EXEC-DAG: GPR[3] = 0xffffffffffffff00
# EXEC-DAG: GPR[8] = 0xffffffffffffff01
