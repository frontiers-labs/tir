void *malloc(unsigned long size);
void free(void *pointer);

struct Node {
    int value;
    struct Node *next;
};

struct Node *push(struct Node *head, int value) {
    struct Node *node = malloc(sizeof(struct Node));
    node->value = value;
    node->next = head;
    return node;
}

struct Node *reverse(struct Node *head) {
    struct Node *reversed = 0;
    while (head) {
        struct Node *next = head->next;
        head->next = reversed;
        reversed = head;
        head = next;
    }
    return reversed;
}

int list_checksum(struct Node *head) {
    int checksum = 0;
    int position = 1;
    while (head) {
        checksum += position * head->value;
        position++;
        head = head->next;
    }
    return checksum;
}

void destroy(struct Node *head) {
    while (head) {
        struct Node *next = head->next;
        free(head);
        head = next;
    }
}

int main(void) {
    struct Node *head = 0;
    head = push(head, 3);
    head = push(head, 7);
    head = push(head, 11);
    head = push(head, 21);

    int before = list_checksum(head);
    head = reverse(head);
    int after = list_checksum(head);
    destroy(head);
    if (before != 76)
        return 1;
    if (after != 134)
        return 2;
    return 0;
}
