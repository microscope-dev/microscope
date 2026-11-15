#include "shapes.h"

namespace shapes {

int shapes_measured = 0;

__attribute__((visibility("default"), noinline)) std::int64_t measure(Shape &shape) {
    std::int64_t measured = area(shape) + width(shape);
    shape.shade = measured > 10 ? Shade::Dark : Shade::Light;
    shapes_measured += 1; // shapes: measured
    return measured;
}

}
