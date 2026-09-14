/* Keywords are preprocessing identifiers until expansion finishes. */
#define false 0
#define true (!false)
#define VALUE(false) false

#if !defined(false) || !defined(true)
#error keyword macros must be defined
#endif

int main(void) {
    return false || !true || VALUE(7) != 7;
}
