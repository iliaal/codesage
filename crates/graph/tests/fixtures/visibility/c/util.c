#include "clamp.h"

static int helper(int x) { return x + 1; }

int util_entry(int x) { return helper(clamp(x)); }
