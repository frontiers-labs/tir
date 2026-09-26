#include <string.h>
#include <stdint.h>

extern float _Complex id_complex(float _Complex value);
extern float _Complex load_complex(const float _Complex *value);
extern void store_complex(float _Complex *address, float _Complex value);
extern int complex_layout(void);

int main(void) {
    float parts[2] = {1.25f, -2.5f};
    float _Complex input;
    memcpy(&input, parts, sizeof input);
    float _Complex result = id_complex(input);
    if (!complex_layout() || memcmp(&result, parts, sizeof result) != 0)
        return 1;
    float _Complex stored = 0;
    store_complex(&stored, input);
    if (memcmp(&stored, parts, sizeof stored) != 0)
        return 2;
    result = load_complex(&stored);
    if (memcmp(&result, parts, sizeof result) != 0)
        return 3;
    uint32_t payloads[2] = {0x7fc12345u, 0x80000000u};
    float _Complex payload;
    memcpy(&payload, payloads, sizeof payload);
    result = id_complex(payload);
    if (memcmp(&result, payloads, sizeof result) != 0)
        return 4;
    store_complex(&stored, payload);
    result = load_complex(&stored);
    return memcmp(&result, payloads, sizeof result) != 0;
}
