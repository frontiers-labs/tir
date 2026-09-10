# RUN: tir mc --march=rv32ifd %s | filecheck %s
# RUN: tir mc --march=rv32ifd --filetype=obj-ascii %s | filecheck --check-prefix=ENC %s

.global rounded
rounded:
fadd.s f10, f11, f12, rne
# CHECK: fadd.s f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x00]
fadd.s f10, f11, f12, rtz
# CHECK: fadd.s f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x00]
fadd.s f10, f11, f12, rdn
# CHECK: fadd.s f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x00]
fadd.s f10, f11, f12, rup
# CHECK: fadd.s f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x00]
fadd.s f10, f11, f12, rmm
# CHECK: fadd.s f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x00]
fadd.s f10, f11, f12
# CHECK: fadd.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x00]
fadd.s f10, f11, f12, dyn
# CHECK: fadd.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x00]
fsub.s f10, f11, f12, rne
# CHECK: fsub.s f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x08]
fsub.s f10, f11, f12, rtz
# CHECK: fsub.s f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x08]
fsub.s f10, f11, f12, rdn
# CHECK: fsub.s f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x08]
fsub.s f10, f11, f12, rup
# CHECK: fsub.s f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x08]
fsub.s f10, f11, f12, rmm
# CHECK: fsub.s f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x08]
fsub.s f10, f11, f12
# CHECK: fsub.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x08]
fsub.s f10, f11, f12, dyn
# CHECK: fsub.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x08]
fmul.s f10, f11, f12, rne
# CHECK: fmul.s f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x10]
fmul.s f10, f11, f12, rtz
# CHECK: fmul.s f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x10]
fmul.s f10, f11, f12, rdn
# CHECK: fmul.s f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x10]
fmul.s f10, f11, f12, rup
# CHECK: fmul.s f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x10]
fmul.s f10, f11, f12, rmm
# CHECK: fmul.s f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x10]
fmul.s f10, f11, f12
# CHECK: fmul.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x10]
fmul.s f10, f11, f12, dyn
# CHECK: fmul.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x10]
fdiv.s f10, f11, f12, rne
# CHECK: fdiv.s f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x18]
fdiv.s f10, f11, f12, rtz
# CHECK: fdiv.s f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x18]
fdiv.s f10, f11, f12, rdn
# CHECK: fdiv.s f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x18]
fdiv.s f10, f11, f12, rup
# CHECK: fdiv.s f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x18]
fdiv.s f10, f11, f12, rmm
# CHECK: fdiv.s f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x18]
fdiv.s f10, f11, f12
# CHECK: fdiv.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x18]
fdiv.s f10, f11, f12, dyn
# CHECK: fdiv.s f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x18]
fmadd.s f10, f11, f12, f13, rne
# CHECK: fmadd.s f10, f11, f12, f13, rne
# ENC: [0x43, 0x85, 0xC5, 0x68]
fmadd.s f10, f11, f12, f13, rtz
# CHECK: fmadd.s f10, f11, f12, f13, rtz
# ENC: [0x43, 0x95, 0xC5, 0x68]
fmadd.s f10, f11, f12, f13, rdn
# CHECK: fmadd.s f10, f11, f12, f13, rdn
# ENC: [0x43, 0xA5, 0xC5, 0x68]
fmadd.s f10, f11, f12, f13, rup
# CHECK: fmadd.s f10, f11, f12, f13, rup
# ENC: [0x43, 0xB5, 0xC5, 0x68]
fmadd.s f10, f11, f12, f13, rmm
# CHECK: fmadd.s f10, f11, f12, f13, rmm
# ENC: [0x43, 0xC5, 0xC5, 0x68]
fmadd.s f10, f11, f12, f13
# CHECK: fmadd.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x43, 0xF5, 0xC5, 0x68]
fmadd.s f10, f11, f12, f13, dyn
# CHECK: fmadd.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x43, 0xF5, 0xC5, 0x68]
fmsub.s f10, f11, f12, f13, rne
# CHECK: fmsub.s f10, f11, f12, f13, rne
# ENC: [0x47, 0x85, 0xC5, 0x68]
fmsub.s f10, f11, f12, f13, rtz
# CHECK: fmsub.s f10, f11, f12, f13, rtz
# ENC: [0x47, 0x95, 0xC5, 0x68]
fmsub.s f10, f11, f12, f13, rdn
# CHECK: fmsub.s f10, f11, f12, f13, rdn
# ENC: [0x47, 0xA5, 0xC5, 0x68]
fmsub.s f10, f11, f12, f13, rup
# CHECK: fmsub.s f10, f11, f12, f13, rup
# ENC: [0x47, 0xB5, 0xC5, 0x68]
fmsub.s f10, f11, f12, f13, rmm
# CHECK: fmsub.s f10, f11, f12, f13, rmm
# ENC: [0x47, 0xC5, 0xC5, 0x68]
fmsub.s f10, f11, f12, f13
# CHECK: fmsub.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x47, 0xF5, 0xC5, 0x68]
fmsub.s f10, f11, f12, f13, dyn
# CHECK: fmsub.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x47, 0xF5, 0xC5, 0x68]
fnmsub.s f10, f11, f12, f13, rne
# CHECK: fnmsub.s f10, f11, f12, f13, rne
# ENC: [0x4B, 0x85, 0xC5, 0x68]
fnmsub.s f10, f11, f12, f13, rtz
# CHECK: fnmsub.s f10, f11, f12, f13, rtz
# ENC: [0x4B, 0x95, 0xC5, 0x68]
fnmsub.s f10, f11, f12, f13, rdn
# CHECK: fnmsub.s f10, f11, f12, f13, rdn
# ENC: [0x4B, 0xA5, 0xC5, 0x68]
fnmsub.s f10, f11, f12, f13, rup
# CHECK: fnmsub.s f10, f11, f12, f13, rup
# ENC: [0x4B, 0xB5, 0xC5, 0x68]
fnmsub.s f10, f11, f12, f13, rmm
# CHECK: fnmsub.s f10, f11, f12, f13, rmm
# ENC: [0x4B, 0xC5, 0xC5, 0x68]
fnmsub.s f10, f11, f12, f13
# CHECK: fnmsub.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4B, 0xF5, 0xC5, 0x68]
fnmsub.s f10, f11, f12, f13, dyn
# CHECK: fnmsub.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4B, 0xF5, 0xC5, 0x68]
fnmadd.s f10, f11, f12, f13, rne
# CHECK: fnmadd.s f10, f11, f12, f13, rne
# ENC: [0x4F, 0x85, 0xC5, 0x68]
fnmadd.s f10, f11, f12, f13, rtz
# CHECK: fnmadd.s f10, f11, f12, f13, rtz
# ENC: [0x4F, 0x95, 0xC5, 0x68]
fnmadd.s f10, f11, f12, f13, rdn
# CHECK: fnmadd.s f10, f11, f12, f13, rdn
# ENC: [0x4F, 0xA5, 0xC5, 0x68]
fnmadd.s f10, f11, f12, f13, rup
# CHECK: fnmadd.s f10, f11, f12, f13, rup
# ENC: [0x4F, 0xB5, 0xC5, 0x68]
fnmadd.s f10, f11, f12, f13, rmm
# CHECK: fnmadd.s f10, f11, f12, f13, rmm
# ENC: [0x4F, 0xC5, 0xC5, 0x68]
fnmadd.s f10, f11, f12, f13
# CHECK: fnmadd.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4F, 0xF5, 0xC5, 0x68]
fnmadd.s f10, f11, f12, f13, dyn
# CHECK: fnmadd.s f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4F, 0xF5, 0xC5, 0x68]
fsqrt.s f10, f11, rne
# CHECK: fsqrt.s f10, f11, rne
# ENC: [0x53, 0x85, 0x05, 0x58]
fsqrt.s f10, f11, rtz
# CHECK: fsqrt.s f10, f11, rtz
# ENC: [0x53, 0x95, 0x05, 0x58]
fsqrt.s f10, f11, rdn
# CHECK: fsqrt.s f10, f11, rdn
# ENC: [0x53, 0xA5, 0x05, 0x58]
fsqrt.s f10, f11, rup
# CHECK: fsqrt.s f10, f11, rup
# ENC: [0x53, 0xB5, 0x05, 0x58]
fsqrt.s f10, f11, rmm
# CHECK: fsqrt.s f10, f11, rmm
# ENC: [0x53, 0xC5, 0x05, 0x58]
fsqrt.s f10, f11
# CHECK: fsqrt.s f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0x58]
fsqrt.s f10, f11, dyn
# CHECK: fsqrt.s f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0x58]
fcvt.s.w f10, x11, rne
# CHECK: fcvt.s.w f10, x11, rne
# ENC: [0x53, 0x85, 0x05, 0xD0]
fcvt.s.w f10, x11, rtz
# CHECK: fcvt.s.w f10, x11, rtz
# ENC: [0x53, 0x95, 0x05, 0xD0]
fcvt.s.w f10, x11, rdn
# CHECK: fcvt.s.w f10, x11, rdn
# ENC: [0x53, 0xA5, 0x05, 0xD0]
fcvt.s.w f10, x11, rup
# CHECK: fcvt.s.w f10, x11, rup
# ENC: [0x53, 0xB5, 0x05, 0xD0]
fcvt.s.w f10, x11, rmm
# CHECK: fcvt.s.w f10, x11, rmm
# ENC: [0x53, 0xC5, 0x05, 0xD0]
fcvt.s.w f10, x11
# CHECK: fcvt.s.w f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xD0]
fcvt.s.w f10, x11, dyn
# CHECK: fcvt.s.w f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xD0]
fcvt.w.s x10, f11, rne
# CHECK: fcvt.w.s x10, f11, rne
# ENC: [0x53, 0x85, 0x05, 0xC0]
fcvt.w.s x10, f11, rtz
# CHECK: fcvt.w.s x10, f11, rtz
# ENC: [0x53, 0x95, 0x05, 0xC0]
fcvt.w.s x10, f11, rdn
# CHECK: fcvt.w.s x10, f11, rdn
# ENC: [0x53, 0xA5, 0x05, 0xC0]
fcvt.w.s x10, f11, rup
# CHECK: fcvt.w.s x10, f11, rup
# ENC: [0x53, 0xB5, 0x05, 0xC0]
fcvt.w.s x10, f11, rmm
# CHECK: fcvt.w.s x10, f11, rmm
# ENC: [0x53, 0xC5, 0x05, 0xC0]
fcvt.w.s x10, f11
# CHECK: fcvt.w.s x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xC0]
fcvt.w.s x10, f11, dyn
# CHECK: fcvt.w.s x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xC0]
fcvt.s.wu f10, x11, rne
# CHECK: fcvt.s.wu f10, x11, rne
# ENC: [0x53, 0x85, 0x15, 0xD0]
fcvt.s.wu f10, x11, rtz
# CHECK: fcvt.s.wu f10, x11, rtz
# ENC: [0x53, 0x95, 0x15, 0xD0]
fcvt.s.wu f10, x11, rdn
# CHECK: fcvt.s.wu f10, x11, rdn
# ENC: [0x53, 0xA5, 0x15, 0xD0]
fcvt.s.wu f10, x11, rup
# CHECK: fcvt.s.wu f10, x11, rup
# ENC: [0x53, 0xB5, 0x15, 0xD0]
fcvt.s.wu f10, x11, rmm
# CHECK: fcvt.s.wu f10, x11, rmm
# ENC: [0x53, 0xC5, 0x15, 0xD0]
fcvt.s.wu f10, x11
# CHECK: fcvt.s.wu f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xD0]
fcvt.s.wu f10, x11, dyn
# CHECK: fcvt.s.wu f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xD0]
fcvt.wu.s x10, f11, rne
# CHECK: fcvt.wu.s x10, f11, rne
# ENC: [0x53, 0x85, 0x15, 0xC0]
fcvt.wu.s x10, f11, rtz
# CHECK: fcvt.wu.s x10, f11, rtz
# ENC: [0x53, 0x95, 0x15, 0xC0]
fcvt.wu.s x10, f11, rdn
# CHECK: fcvt.wu.s x10, f11, rdn
# ENC: [0x53, 0xA5, 0x15, 0xC0]
fcvt.wu.s x10, f11, rup
# CHECK: fcvt.wu.s x10, f11, rup
# ENC: [0x53, 0xB5, 0x15, 0xC0]
fcvt.wu.s x10, f11, rmm
# CHECK: fcvt.wu.s x10, f11, rmm
# ENC: [0x53, 0xC5, 0x15, 0xC0]
fcvt.wu.s x10, f11
# CHECK: fcvt.wu.s x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xC0]
fcvt.wu.s x10, f11, dyn
# CHECK: fcvt.wu.s x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xC0]
fcvt.s.d f10, f11, rne
# CHECK: fcvt.s.d f10, f11, rne
# ENC: [0x53, 0x85, 0x15, 0x40]
fcvt.s.d f10, f11, rtz
# CHECK: fcvt.s.d f10, f11, rtz
# ENC: [0x53, 0x95, 0x15, 0x40]
fcvt.s.d f10, f11, rdn
# CHECK: fcvt.s.d f10, f11, rdn
# ENC: [0x53, 0xA5, 0x15, 0x40]
fcvt.s.d f10, f11, rup
# CHECK: fcvt.s.d f10, f11, rup
# ENC: [0x53, 0xB5, 0x15, 0x40]
fcvt.s.d f10, f11, rmm
# CHECK: fcvt.s.d f10, f11, rmm
# ENC: [0x53, 0xC5, 0x15, 0x40]
fcvt.s.d f10, f11
# CHECK: fcvt.s.d f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0x40]
fcvt.s.d f10, f11, dyn
# CHECK: fcvt.s.d f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0x40]
fadd.d f10, f11, f12, rne
# CHECK: fadd.d f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x02]
fadd.d f10, f11, f12, rtz
# CHECK: fadd.d f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x02]
fadd.d f10, f11, f12, rdn
# CHECK: fadd.d f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x02]
fadd.d f10, f11, f12, rup
# CHECK: fadd.d f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x02]
fadd.d f10, f11, f12, rmm
# CHECK: fadd.d f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x02]
fadd.d f10, f11, f12
# CHECK: fadd.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x02]
fadd.d f10, f11, f12, dyn
# CHECK: fadd.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x02]
fsub.d f10, f11, f12, rne
# CHECK: fsub.d f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x0A]
fsub.d f10, f11, f12, rtz
# CHECK: fsub.d f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x0A]
fsub.d f10, f11, f12, rdn
# CHECK: fsub.d f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x0A]
fsub.d f10, f11, f12, rup
# CHECK: fsub.d f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x0A]
fsub.d f10, f11, f12, rmm
# CHECK: fsub.d f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x0A]
fsub.d f10, f11, f12
# CHECK: fsub.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x0A]
fsub.d f10, f11, f12, dyn
# CHECK: fsub.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x0A]
fmul.d f10, f11, f12, rne
# CHECK: fmul.d f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x12]
fmul.d f10, f11, f12, rtz
# CHECK: fmul.d f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x12]
fmul.d f10, f11, f12, rdn
# CHECK: fmul.d f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x12]
fmul.d f10, f11, f12, rup
# CHECK: fmul.d f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x12]
fmul.d f10, f11, f12, rmm
# CHECK: fmul.d f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x12]
fmul.d f10, f11, f12
# CHECK: fmul.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x12]
fmul.d f10, f11, f12, dyn
# CHECK: fmul.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x12]
fdiv.d f10, f11, f12, rne
# CHECK: fdiv.d f10, f11, f12, rne
# ENC: [0x53, 0x85, 0xC5, 0x1A]
fdiv.d f10, f11, f12, rtz
# CHECK: fdiv.d f10, f11, f12, rtz
# ENC: [0x53, 0x95, 0xC5, 0x1A]
fdiv.d f10, f11, f12, rdn
# CHECK: fdiv.d f10, f11, f12, rdn
# ENC: [0x53, 0xA5, 0xC5, 0x1A]
fdiv.d f10, f11, f12, rup
# CHECK: fdiv.d f10, f11, f12, rup
# ENC: [0x53, 0xB5, 0xC5, 0x1A]
fdiv.d f10, f11, f12, rmm
# CHECK: fdiv.d f10, f11, f12, rmm
# ENC: [0x53, 0xC5, 0xC5, 0x1A]
fdiv.d f10, f11, f12
# CHECK: fdiv.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x1A]
fdiv.d f10, f11, f12, dyn
# CHECK: fdiv.d f10, f11, f12{{(?m)$}}
# ENC: [0x53, 0xF5, 0xC5, 0x1A]
fmadd.d f10, f11, f12, f13, rne
# CHECK: fmadd.d f10, f11, f12, f13, rne
# ENC: [0x43, 0x85, 0xC5, 0x6A]
fmadd.d f10, f11, f12, f13, rtz
# CHECK: fmadd.d f10, f11, f12, f13, rtz
# ENC: [0x43, 0x95, 0xC5, 0x6A]
fmadd.d f10, f11, f12, f13, rdn
# CHECK: fmadd.d f10, f11, f12, f13, rdn
# ENC: [0x43, 0xA5, 0xC5, 0x6A]
fmadd.d f10, f11, f12, f13, rup
# CHECK: fmadd.d f10, f11, f12, f13, rup
# ENC: [0x43, 0xB5, 0xC5, 0x6A]
fmadd.d f10, f11, f12, f13, rmm
# CHECK: fmadd.d f10, f11, f12, f13, rmm
# ENC: [0x43, 0xC5, 0xC5, 0x6A]
fmadd.d f10, f11, f12, f13
# CHECK: fmadd.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x43, 0xF5, 0xC5, 0x6A]
fmadd.d f10, f11, f12, f13, dyn
# CHECK: fmadd.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x43, 0xF5, 0xC5, 0x6A]
fmsub.d f10, f11, f12, f13, rne
# CHECK: fmsub.d f10, f11, f12, f13, rne
# ENC: [0x47, 0x85, 0xC5, 0x6A]
fmsub.d f10, f11, f12, f13, rtz
# CHECK: fmsub.d f10, f11, f12, f13, rtz
# ENC: [0x47, 0x95, 0xC5, 0x6A]
fmsub.d f10, f11, f12, f13, rdn
# CHECK: fmsub.d f10, f11, f12, f13, rdn
# ENC: [0x47, 0xA5, 0xC5, 0x6A]
fmsub.d f10, f11, f12, f13, rup
# CHECK: fmsub.d f10, f11, f12, f13, rup
# ENC: [0x47, 0xB5, 0xC5, 0x6A]
fmsub.d f10, f11, f12, f13, rmm
# CHECK: fmsub.d f10, f11, f12, f13, rmm
# ENC: [0x47, 0xC5, 0xC5, 0x6A]
fmsub.d f10, f11, f12, f13
# CHECK: fmsub.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x47, 0xF5, 0xC5, 0x6A]
fmsub.d f10, f11, f12, f13, dyn
# CHECK: fmsub.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x47, 0xF5, 0xC5, 0x6A]
fnmsub.d f10, f11, f12, f13, rne
# CHECK: fnmsub.d f10, f11, f12, f13, rne
# ENC: [0x4B, 0x85, 0xC5, 0x6A]
fnmsub.d f10, f11, f12, f13, rtz
# CHECK: fnmsub.d f10, f11, f12, f13, rtz
# ENC: [0x4B, 0x95, 0xC5, 0x6A]
fnmsub.d f10, f11, f12, f13, rdn
# CHECK: fnmsub.d f10, f11, f12, f13, rdn
# ENC: [0x4B, 0xA5, 0xC5, 0x6A]
fnmsub.d f10, f11, f12, f13, rup
# CHECK: fnmsub.d f10, f11, f12, f13, rup
# ENC: [0x4B, 0xB5, 0xC5, 0x6A]
fnmsub.d f10, f11, f12, f13, rmm
# CHECK: fnmsub.d f10, f11, f12, f13, rmm
# ENC: [0x4B, 0xC5, 0xC5, 0x6A]
fnmsub.d f10, f11, f12, f13
# CHECK: fnmsub.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4B, 0xF5, 0xC5, 0x6A]
fnmsub.d f10, f11, f12, f13, dyn
# CHECK: fnmsub.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4B, 0xF5, 0xC5, 0x6A]
fnmadd.d f10, f11, f12, f13, rne
# CHECK: fnmadd.d f10, f11, f12, f13, rne
# ENC: [0x4F, 0x85, 0xC5, 0x6A]
fnmadd.d f10, f11, f12, f13, rtz
# CHECK: fnmadd.d f10, f11, f12, f13, rtz
# ENC: [0x4F, 0x95, 0xC5, 0x6A]
fnmadd.d f10, f11, f12, f13, rdn
# CHECK: fnmadd.d f10, f11, f12, f13, rdn
# ENC: [0x4F, 0xA5, 0xC5, 0x6A]
fnmadd.d f10, f11, f12, f13, rup
# CHECK: fnmadd.d f10, f11, f12, f13, rup
# ENC: [0x4F, 0xB5, 0xC5, 0x6A]
fnmadd.d f10, f11, f12, f13, rmm
# CHECK: fnmadd.d f10, f11, f12, f13, rmm
# ENC: [0x4F, 0xC5, 0xC5, 0x6A]
fnmadd.d f10, f11, f12, f13
# CHECK: fnmadd.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4F, 0xF5, 0xC5, 0x6A]
fnmadd.d f10, f11, f12, f13, dyn
# CHECK: fnmadd.d f10, f11, f12, f13{{(?m)$}}
# ENC: [0x4F, 0xF5, 0xC5, 0x6A]
fsqrt.d f10, f11, rne
# CHECK: fsqrt.d f10, f11, rne
# ENC: [0x53, 0x85, 0x05, 0x5A]
fsqrt.d f10, f11, rtz
# CHECK: fsqrt.d f10, f11, rtz
# ENC: [0x53, 0x95, 0x05, 0x5A]
fsqrt.d f10, f11, rdn
# CHECK: fsqrt.d f10, f11, rdn
# ENC: [0x53, 0xA5, 0x05, 0x5A]
fsqrt.d f10, f11, rup
# CHECK: fsqrt.d f10, f11, rup
# ENC: [0x53, 0xB5, 0x05, 0x5A]
fsqrt.d f10, f11, rmm
# CHECK: fsqrt.d f10, f11, rmm
# ENC: [0x53, 0xC5, 0x05, 0x5A]
fsqrt.d f10, f11
# CHECK: fsqrt.d f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0x5A]
fsqrt.d f10, f11, dyn
# CHECK: fsqrt.d f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0x5A]
fcvt.d.w f10, x11, rne
# CHECK: fcvt.d.w f10, x11, rne
# ENC: [0x53, 0x85, 0x05, 0xD2]
fcvt.d.w f10, x11, rtz
# CHECK: fcvt.d.w f10, x11, rtz
# ENC: [0x53, 0x95, 0x05, 0xD2]
fcvt.d.w f10, x11, rdn
# CHECK: fcvt.d.w f10, x11, rdn
# ENC: [0x53, 0xA5, 0x05, 0xD2]
fcvt.d.w f10, x11, rup
# CHECK: fcvt.d.w f10, x11, rup
# ENC: [0x53, 0xB5, 0x05, 0xD2]
fcvt.d.w f10, x11, rmm
# CHECK: fcvt.d.w f10, x11, rmm
# ENC: [0x53, 0xC5, 0x05, 0xD2]
fcvt.d.w f10, x11
# CHECK: fcvt.d.w f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xD2]
fcvt.d.w f10, x11, dyn
# CHECK: fcvt.d.w f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xD2]
fcvt.w.d x10, f11, rne
# CHECK: fcvt.w.d x10, f11, rne
# ENC: [0x53, 0x85, 0x05, 0xC2]
fcvt.w.d x10, f11, rtz
# CHECK: fcvt.w.d x10, f11, rtz
# ENC: [0x53, 0x95, 0x05, 0xC2]
fcvt.w.d x10, f11, rdn
# CHECK: fcvt.w.d x10, f11, rdn
# ENC: [0x53, 0xA5, 0x05, 0xC2]
fcvt.w.d x10, f11, rup
# CHECK: fcvt.w.d x10, f11, rup
# ENC: [0x53, 0xB5, 0x05, 0xC2]
fcvt.w.d x10, f11, rmm
# CHECK: fcvt.w.d x10, f11, rmm
# ENC: [0x53, 0xC5, 0x05, 0xC2]
fcvt.w.d x10, f11
# CHECK: fcvt.w.d x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xC2]
fcvt.w.d x10, f11, dyn
# CHECK: fcvt.w.d x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0xC2]
fcvt.d.wu f10, x11, rne
# CHECK: fcvt.d.wu f10, x11, rne
# ENC: [0x53, 0x85, 0x15, 0xD2]
fcvt.d.wu f10, x11, rtz
# CHECK: fcvt.d.wu f10, x11, rtz
# ENC: [0x53, 0x95, 0x15, 0xD2]
fcvt.d.wu f10, x11, rdn
# CHECK: fcvt.d.wu f10, x11, rdn
# ENC: [0x53, 0xA5, 0x15, 0xD2]
fcvt.d.wu f10, x11, rup
# CHECK: fcvt.d.wu f10, x11, rup
# ENC: [0x53, 0xB5, 0x15, 0xD2]
fcvt.d.wu f10, x11, rmm
# CHECK: fcvt.d.wu f10, x11, rmm
# ENC: [0x53, 0xC5, 0x15, 0xD2]
fcvt.d.wu f10, x11
# CHECK: fcvt.d.wu f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xD2]
fcvt.d.wu f10, x11, dyn
# CHECK: fcvt.d.wu f10, x11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xD2]
fcvt.wu.d x10, f11, rne
# CHECK: fcvt.wu.d x10, f11, rne
# ENC: [0x53, 0x85, 0x15, 0xC2]
fcvt.wu.d x10, f11, rtz
# CHECK: fcvt.wu.d x10, f11, rtz
# ENC: [0x53, 0x95, 0x15, 0xC2]
fcvt.wu.d x10, f11, rdn
# CHECK: fcvt.wu.d x10, f11, rdn
# ENC: [0x53, 0xA5, 0x15, 0xC2]
fcvt.wu.d x10, f11, rup
# CHECK: fcvt.wu.d x10, f11, rup
# ENC: [0x53, 0xB5, 0x15, 0xC2]
fcvt.wu.d x10, f11, rmm
# CHECK: fcvt.wu.d x10, f11, rmm
# ENC: [0x53, 0xC5, 0x15, 0xC2]
fcvt.wu.d x10, f11
# CHECK: fcvt.wu.d x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xC2]
fcvt.wu.d x10, f11, dyn
# CHECK: fcvt.wu.d x10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x15, 0xC2]
fcvt.d.s f10, f11, rne
# CHECK: fcvt.d.s f10, f11, rne
# ENC: [0x53, 0x85, 0x05, 0x42]
fcvt.d.s f10, f11, rtz
# CHECK: fcvt.d.s f10, f11, rtz
# ENC: [0x53, 0x95, 0x05, 0x42]
fcvt.d.s f10, f11, rdn
# CHECK: fcvt.d.s f10, f11, rdn
# ENC: [0x53, 0xA5, 0x05, 0x42]
fcvt.d.s f10, f11, rup
# CHECK: fcvt.d.s f10, f11, rup
# ENC: [0x53, 0xB5, 0x05, 0x42]
fcvt.d.s f10, f11, rmm
# CHECK: fcvt.d.s f10, f11, rmm
# ENC: [0x53, 0xC5, 0x05, 0x42]
fcvt.d.s f10, f11
# CHECK: fcvt.d.s f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0x42]
fcvt.d.s f10, f11, dyn
# CHECK: fcvt.d.s f10, f11{{(?m)$}}
# ENC: [0x53, 0xF5, 0x05, 0x42]
