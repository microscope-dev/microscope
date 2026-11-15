


#pragma once

#include <cstdint>

namespace shapes {

enum class Shade : std::uint8_t { Light, Dark };

struct Point {
    std::int32_t x;
    std::int32_t y;
};

template <typename T>
struct Span {
    T low;
    T high;
    T length() const { return high - low; }
};

class Shape {
public:
    Shape(const char *name, Point corner, Point opposite)
        : name_(name), corner_(corner), opposite_(opposite) {}
    const char *name() const { return name_; }
    Span<std::int32_t> across() const { return {corner_.x, opposite_.x}; }
    Span<std::int32_t> down() const { return {corner_.y, opposite_.y}; }
    Shade shade = Shade::Light;

private:
    const char *name_;
    Point corner_;
    Point opposite_;
};

inline std::int32_t width(const Shape &shape) {
    Span<std::int32_t> across = shape.across();
    return across.length(); // shapes: width
}

std::int64_t area(const Shape &shape);

extern int shapes_measured;

std::int64_t measure(Shape &shape);

}
