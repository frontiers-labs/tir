# pass=instcombine nodes=206 classes=200 rules=46/85 skipped={"fact atom": 2, "guard": 32}
n 0 Symbol:v990
n 1 Symbol:v991
n 2 Symbol:v992
n 3 Symbol:v993
n 4 Symbol:v994
n 5 Symbol:v995
n 6 Symbol:v996
n 7 Symbol:v1011
n 8 Symbol:v176
n 9 Symbol:v177
n 10 Symbol:v178
n 11 Constant:8
n 12 Constant:0
n 13 StoreMemory 10 11 1 12 6
n 14 Constant:0
n 15 Symbol:v2387
n 15 Port 15
n 16 Symbol:v1012
n 17 Symbol:v1013
n 18 Symbol:v1015
n 19 Constant:8
n 20 builtin.cmpi#7f533c4 15 19
n 21 builtin.extui 15
n 22 Constant:2
n 23 builtin.shli 21 22
n 24 ptr.ptradd 8 23
n 25 ptr.ptradd 9 23
n 26 state.join#52df9aaa 17 18
n 27 Constant:4
n 28 StoreMemory 25 27 14 12 26
n 29 StoreMemory 24 27 14 12 16
n 30 Constant:1
n 31 builtin.addi 15 30
n 32 If 20 31 15
n 33 Symbol:v1027
n 34 Symbol:v1028
n 35 Symbol:v1030
n 37 Symbol:v2389
n 37 Loop 14 32 32 20
n 39 Symbol:v1031
n 40 Symbol:v1032
n 41 Symbol:v1034
n 42 Symbol:v1035
n 43 Symbol:v1036
n 44 Symbol:v1037
n 45 Symbol:v1039
n 46 Symbol:v1040
n 47 LoadMemory 10 11 12 44
n 48 Constant:1
n 49 LoadMemory 47 48 12 45
n 50 builtin.extui 49
n 51 builtin.cmpi#e0d3292f 50 14
n 52 state.join#7a01a53f 45 43 44
n 53 Symbol:v208
n 54 Symbol:v831
n 55 Symbol:v832
n 56 builtin.extsi 53
n 57 builtin.shli 56 22
n 58 ptr.ptradd 8 57
n 59 LoadMemory 58 27 12 42
n 60 builtin.addi 30 59
n 61 StoreMemory 58 27 60 12 42
n 62 Symbol:v1056
n 63 Symbol:v1057
n 64 Symbol:v1058
n 65 Symbol:v1060
n 66 Symbol:v1061
n 67 Symbol:v1062
n 68 Symbol:v1063
n 69 Symbol:v1064
n 70 Symbol:v1066
n 71 Symbol:v1067
n 72 state.join#52df9aaa 69 70
n 73 StoreMemory 10 11 1 12 72
n 74 Symbol:v1072
n 75 Symbol:v1073
n 76 Symbol:v1074
n 77 builtin.extui 0
n 78 ptr.ptradd 1 77
n 79 LoadMemory 10 11 12 75
n 80 ptr.cmp#78ee482d 79 78
n 81 LoadMemory 79 48 12 76
n 82 builtin.extui 81
n 83 Constant:44
n 84 builtin.cmpi#e0d3292f 82 83
n 85 builtin.trunci 2
n 86 builtin.extui 85
n 87 builtin.xori 82 86
n 88 builtin.trunci 87
n 89 state.join#7a01a53f 76 74 75
n 90 StoreMemory 79 48 88 12 89
n 91 Symbol:v1096
n 92 Symbol:v1097
n 93 Symbol:v1098
n 94 LoadMemory 10 11 12 92
n 95 builtin.extsi 4
n 96 ptr.ptradd 94 95
n 97 state.join#52df9aaa 92 93
n 98 StoreMemory 10 11 96 12 97
n 99 Symbol:v1102
n 100 Symbol:v1103
n 101 Symbol:v1104
n 102 Symbol:v1109
n 103 Symbol:v1110
n 104 Symbol:v1111
n 105 state.join#52df9aaa 103 104
n 106 StoreMemory 10 11 1 12 105
n 107 Symbol:v1112
n 108 Symbol:v1113
n 109 Symbol:v1114
n 110 Symbol:v1116
n 111 Symbol:v1117
n 112 LoadMemory 10 11 12 109
n 113 LoadMemory 112 48 12 110
n 114 builtin.extui 113
n 115 builtin.cmpi#e0d3292f 114 14
n 116 state.join#7a01a53f 110 108 109
n 117 Symbol:v251
n 118 Symbol:v885
n 119 Symbol:v886
n 120 builtin.extsi 117
n 121 builtin.shli 120 22
n 122 ptr.ptradd 8 121
n 123 LoadMemory 122 27 12 107
n 124 builtin.addi 30 123
n 125 StoreMemory 122 27 124 12 107
n 126 Symbol:v1133
n 127 Symbol:v1134
n 128 Symbol:v1135
n 129 Symbol:v1137
n 130 Symbol:v1138
n 131 Symbol:v1139
n 132 Symbol:v1140
n 133 Symbol:v1141
n 134 Symbol:v1143
n 135 Symbol:v1144
n 136 state.join#52df9aaa 133 134
n 137 StoreMemory 10 11 1 12 136
n 138 Symbol:v1149
n 139 Symbol:v1150
n 140 Symbol:v1151
n 141 LoadMemory 10 11 12 139
n 142 ptr.cmp#78ee482d 141 78
n 143 LoadMemory 141 48 12 140
n 144 builtin.extui 143
n 145 builtin.cmpi#e0d3292f 144 83
n 146 builtin.trunci 3
n 147 builtin.extui 146
n 148 builtin.xori 144 147
n 149 builtin.trunci 148
n 150 state.join#7a01a53f 140 138 139
n 151 StoreMemory 141 48 149 12 150
n 152 Symbol:v1173
n 153 Symbol:v1174
n 154 Symbol:v1175
n 155 LoadMemory 10 11 12 153
n 156 ptr.ptradd 155 95
n 157 state.join#52df9aaa 153 154
n 158 StoreMemory 10 11 156 12 157
n 159 Symbol:v1179
n 160 Symbol:v1180
n 161 Symbol:v1181
n 162 Symbol:v1186
n 163 Symbol:v1187
n 164 Symbol:v1188
n 165 Symbol:v2384
n 165 Port 165
n 166 Symbol:v2390
n 166 Port 166
n 167 Symbol:v1190
n 168 Symbol:v1191
n 169 Symbol:v1192
n 170 Symbol:v1194
n 171 Symbol:v1195
n 172 builtin.cmpi#7f533c4 166 19
n 173 builtin.extui 166
n 174 builtin.shli 173 22
n 175 ptr.ptradd 8 174
n 176 LoadMemory 175 27 12 167
n 177 state.join#7a01a53f 170 168 169
n 178 Symbol:v300
n 179 Symbol:v940
n 180 Symbol:v941
n 181 ptr.ptradd 9 174
n 182 LoadMemory 181 27 12 179
n 183 Symbol:v308
n 184 Symbol:v950
n 185 Symbol:v951
n 186 builtin.addi 30 166
n 187 If 172 183 165
n 188 If 172 186 166
n 189 Symbol:v1214
n 190 Symbol:v1215
n 191 Symbol:v1216
n 192 Symbol:v1218
n 193 Symbol:v1219
n 195 Symbol:v2386
n 195 Loop 5 187 187 172
n 198 Symbol:v2392
n 198 Loop 14 188 188 172
n 200 Symbol:v1221
n 201 Symbol:v1222
n 202 Symbol:v1223
n 203 Symbol:v1225
n 204 Symbol:v1226
n 205 state.join#287819a6 6 6 6 6 6 6 200 201 202 6 6 6 6 203
c builtin.addi
c builtin.xori
c state.join#52df9aaa
r add-zero (builtin.addi ?v0 Constant:0) => ?v0
r mul-one (builtin.muli ?v0 Constant:1) => ?v0
r mul-zero (builtin.muli ?v0 Constant:0) => Constant:k.mul-zero.0
r sub-self (builtin.subi ?v0 ?v0) => Constant:k.sub-self.0
r sub-zero (builtin.subi ?v0 Constant:0) => ?v0
r sub-add-cancel (builtin.subi (builtin.addi ?v0 ?v1) ?v1) => ?v0
r add-sub-cancel (builtin.addi (builtin.subi ?v0 ?v1) ?v1) => ?v0
r sub-sub-cancel (builtin.subi ?v0 (builtin.subi ?v0 ?v1)) => ?v1
r add-sub-telescope (builtin.addi (builtin.subi ?v0 ?v1) (builtin.subi ?v1 ?v3)) => (builtin.subi ?v0 ?v3)
r sub-add-negate-right (builtin.subi ?v0 (builtin.addi ?v0 ?v1)) => (builtin.subi Constant:k.sub-add-negate-right.0 ?v1)
r sub-sub-left (builtin.subi (builtin.subi ?v0 ?v1) ?v0) => (builtin.subi Constant:k.sub-sub-left.0 ?v1)
r sub-common-left (builtin.subi (builtin.subi ?v0 ?v1) (builtin.subi ?v0 ?v3)) => (builtin.subi ?v3 ?v1)
r sub-common-right (builtin.subi (builtin.subi ?v0 ?v1) (builtin.subi ?v3 ?v1)) => (builtin.subi ?v0 ?v3)
r sub-add-common-left (builtin.subi (builtin.addi ?v0 ?v1) (builtin.addi ?v0 ?v3)) => (builtin.subi ?v1 ?v3)
r add-add-sub-cancel (builtin.addi (builtin.addi ?v0 ?v1) (builtin.subi ?v3 ?v1)) => (builtin.addi ?v0 ?v3)
r udiv-one (builtin.divui ?v0 Constant:1) => ?v0
r sdiv-one (builtin.divsi ?v0 Constant:1) => ?v0
r urem-one (builtin.remui ?v0 Constant:1) => Constant:k.urem-one.0
r srem-one (builtin.remsi ?v0 Constant:1) => Constant:k.srem-one.0
r and-zero (builtin.andi ?v0 Constant:0) => Constant:k.and-zero.0
r and-self (builtin.andi ?v0 ?v0) => ?v0
r or-zero (builtin.ori ?v0 Constant:0) => ?v0
r or-self (builtin.ori ?v0 ?v0) => ?v0
r xor-zero (builtin.xori ?v0 Constant:0) => ?v0
r xor-self (builtin.xori ?v0 ?v0) => Constant:k.xor-self.0
r and-absorption (builtin.andi (builtin.ori ?v0 ?v1) ?v0) => ?v0
r xor-xor-and (builtin.xori (builtin.xori ?v0 ?v1) (builtin.andi ?v0 ?v1)) => (builtin.ori ?v0 ?v1)
r or-absorption (builtin.ori (builtin.andi ?v0 ?v1) ?v0) => ?v0
r xor-cancel (builtin.xori (builtin.xori ?v0 ?v1) ?v1) => ?v0
r shl-zero (builtin.shli ?v0 Constant:0) => ?v0
r shrui-zero (builtin.shrui ?v0 Constant:0) => ?v0
r shrsi-zero (builtin.shrsi ?v0 Constant:0) => ?v0
r xor-or-and (builtin.xori (builtin.ori ?v0 ?v1) (builtin.andi ?v0 ?v1)) => (builtin.xori ?v0 ?v1)
r if-same (If ?v0 ?v1 ?v1) => ?v1
r fp-neg-neg (fp.neg (fp.neg ?v0)) => ?v0
r fp-copysign-self (fp.copysign ?v0 ?v0) => ?v0
r fp-copysign-copysign-magnitude (fp.copysign (fp.copysign ?v0 ?v1) ?v3) => (fp.copysign ?v0 ?v3)
r fp-copysign-copysign-sign (fp.copysign ?v0 (fp.copysign ?v1 ?v2)) => (fp.copysign ?v0 ?v2)
r fp-abs-abs (fp.abs (fp.abs ?v0)) => (fp.abs ?v0)
r fp-abs-neg (fp.abs (fp.neg ?v0)) => (fp.abs ?v0)
r fp-abs-copysign (fp.abs (fp.copysign ?v0 ?v1)) => (fp.abs ?v0)
r fp-copysign-abs-magnitude (fp.copysign (fp.abs ?v0) ?v2) => (fp.copysign ?v0 ?v2)
r fp-copysign-neg-magnitude (fp.copysign (fp.neg ?v0) ?v2) => (fp.copysign ?v0 ?v2)
r fp-copysign-abs-sign (fp.copysign ?v0 (fp.abs ?v1)) => (fp.abs ?v0)
r fp-copysign-neg-self (fp.copysign ?v0 (fp.neg ?v0)) => (fp.neg ?v0)
r fp-neg-copysign-neg-sign (fp.neg (fp.copysign ?v0 (fp.neg ?v1))) => (fp.copysign ?v0 ?v1)
