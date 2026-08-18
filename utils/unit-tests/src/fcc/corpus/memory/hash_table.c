void *malloc(unsigned long size);
void free(void *pointer);

struct Entry {
    int key;
    int value;
    struct Entry *next;
};

struct Table {
    struct Entry *buckets[8];
};

unsigned int bucket_index(int key) {
    return (unsigned int)key % 8u;
}

void table_init(struct Table *table) {
    int index = 0;
    while (index < 8) {
        table->buckets[index] = 0;
        index++;
    }
}

void table_put(struct Table *table, int key, int value) {
    unsigned int index = bucket_index(key);
    struct Entry *entry = table->buckets[index];
    while (entry) {
        if (entry->key == key) {
            entry->value = value;
            return;
        }
        entry = entry->next;
    }

    entry = malloc(sizeof(struct Entry));
    entry->key = key;
    entry->value = value;
    entry->next = table->buckets[index];
    table->buckets[index] = entry;
}

int table_get(struct Table *table, int key, int *result) {
    struct Entry *entry = table->buckets[bucket_index(key)];
    while (entry) {
        if (entry->key == key) {
            *result = entry->value;
            return 1;
        }
        entry = entry->next;
    }
    return 0;
}

void table_destroy(struct Table *table) {
    int index = 0;
    while (index < 8) {
        struct Entry *entry = table->buckets[index];
        while (entry) {
            struct Entry *next = entry->next;
            free(entry);
            entry = next;
        }
        index++;
    }
}

int main(void) {
    struct Table table;
    int value = 0;
    table_init(&table);
    table_put(&table, 1, 11);
    table_put(&table, 9, 22);
    table_put(&table, 17, 33);
    table_put(&table, 9, 42);

    if (!table_get(&table, 1, &value) || value != 11)
        return 1;
    if (!table_get(&table, 9, &value) || value != 42)
        return 2;
    if (!table_get(&table, 17, &value) || value != 33)
        return 3;
    if (table_get(&table, 25, &value))
        return 4;

    table_destroy(&table);
    return 0;
}
