

#include "microscope_views.h"

MICROSCOPE_VIEWS_FILE("tests/fixtures/c/embedded-views/library.views");

struct point {
    int x;
    int y;
};

struct point library_origin = {3, 4};

int library_touch(void) {
    return library_origin.x - 3;
}
