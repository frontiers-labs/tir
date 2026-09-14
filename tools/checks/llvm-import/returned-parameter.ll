; RUN: tir mc %s --march x86_64 --filetype obj-ascii | filecheck %s

define ptr @identity(ptr noundef returned %value) {
  ret ptr %value
}

; CHECK: identity:
