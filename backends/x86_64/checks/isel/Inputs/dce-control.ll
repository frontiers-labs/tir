define ptr @find(ptr %start, i32 %key) {
entry:
  br label %loop
loop:
  %node = phi ptr [ %start, %entry ], [ %next, %step ]
  %addr = getelementptr i8, ptr %node, i64 8
  %value = load i32, ptr %addr
  %hit = icmp eq i32 %value, %key
  br i1 %hit, label %found, label %step
step:
  %next = load ptr, ptr %node
  %done = icmp eq ptr %next, null
  br i1 %done, label %missing, label %loop
found:
  ret ptr %node
missing:
  ret ptr null
}
define i1 @find_data(ptr %start, i32 %key, ptr %out) {
entry:
  br label %loop
loop:
  %node = phi ptr [ %start, %entry ], [ %next, %step ]
  %addr = getelementptr i8, ptr %node, i64 8
  %value = load i32, ptr %addr
  %hit = icmp eq i32 %value, %key
  br i1 %hit, label %found, label %step
step:
  %next = load ptr, ptr %node
  %done = icmp eq ptr %next, null
  %data = zext i1 %done to i32
  store i32 %data, ptr %out
  br i1 %done, label %missing, label %loop
found:
  ret i1 true
missing:
  ret i1 false
}
