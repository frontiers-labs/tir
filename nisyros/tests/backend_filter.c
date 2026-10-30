#include "nisyros.h"
#include <assert.h>
#include <stdio.h>

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_NO_DEVICE);
    assert(cluster == NULL);
    puts("all built-in backends inhibited");
    return 0;
}
