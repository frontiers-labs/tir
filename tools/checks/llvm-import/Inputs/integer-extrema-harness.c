extern int signed_min(int left, int right);
extern int signed_max(int left, int right);
extern unsigned unsigned_min(unsigned left, unsigned right);

int main(void) {
    return signed_min(-3, 2) != -3 || signed_min(2, -3) != -3 ||
           signed_max(-3, 2) != 2 || signed_max(2, -3) != 2 ||
           signed_min(7, 7) != 7 || signed_max(7, 7) != 7 ||
           unsigned_min(1u, 0xffffffffu) != 1u ||
           unsigned_min(0xffffffffu, 1u) != 1u;
}
