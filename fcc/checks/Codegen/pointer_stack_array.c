// RUN: fcc compile --march x86_64 --stage asm -o - %s | filecheck %s

struct item {
  int first;
  int second;
  int third;
  int fourth;
};

int load_second_item(struct item *items)
{
  return items[1].first;
}

int pointer_stack_array(void)
{
  struct item items[2];
  items[0].first = 1;
  items[0].second = 1;
  items[1].first = 3;
  items[1].second = 2;
  return load_second_item(items);
}

// CHECK-LABEL: load_second_item:
// CHECK-LABEL: pointer_stack_array:
