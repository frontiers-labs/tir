// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O0 -o /dev/null %s

#define false 0
#define true (!false)
#define VALUE(false) false

#if !defined(false) || !defined(true)
#error keyword macros must be defined
#endif

int main(void) {
    return false || !true || VALUE(7) != 7;
}
