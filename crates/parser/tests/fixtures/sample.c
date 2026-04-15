#include <stdio.h>
#include <stdlib.h>

#define MAX_BUFFER 1024
#define VERSION "1.0"

typedef unsigned long ulong;

struct config {
    int debug;
    char *name;
};

enum log_level {
    LOG_DEBUG,
    LOG_INFO,
    LOG_ERROR,
};

int add(int a, int b) {
    return a + b;
}

char *get_name(struct config *cfg) {
    return cfg->name;
}

PHP_FUNCTION(parse_url)
{
    char *str;
}
