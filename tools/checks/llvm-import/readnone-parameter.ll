; RUN: tir mc %s --march x86_64 --filetype obj-ascii | filecheck %s

define void @initialize(ptr noundef writeonly %output, ptr noundef readnone %unused) {
  store i8 1, ptr %output
  ret void
}

; CHECK: initialize:
