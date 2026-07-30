// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

int switch_two_cases(short value)
{
    switch (value)
    {
        case 0:
            return 10;
        default:
            return 20;
    }
}

// CHECK: switch_two_cases:
