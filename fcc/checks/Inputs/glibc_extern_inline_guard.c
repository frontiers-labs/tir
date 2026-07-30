#include <features.h>
#include <bits/floatn.h>

#ifdef __OPTIMIZE__
int optimize_defined;
#else
int optimize_undefined;
#endif

#if __HAVE_FLOAT128
int float128_enabled;
#else
int float128_disabled;
#endif

#ifdef __USE_EXTERN_INLINES
int wrong_extern_inline_branch;
#else
int extern_inline_disabled;
#endif
