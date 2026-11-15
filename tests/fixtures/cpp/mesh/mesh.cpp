





#include <cmath>
#include <cstdint>
#include <cstdio>
#include <limits>
#include <vector>

namespace engine {

struct Vec3 {
    float x;
    float y;
    float z;
};

struct Vertex {
    Vec3 position;
    Vec3 normal;
    std::uint8_t color[4];
};

struct Mesh {
    std::vector<Vertex> vertices;
    std::vector<std::uint32_t> indices;
};

struct Bone {
    const char *name;

    int parent;

    float x;
    float y;
};

struct Skeleton {
    Bone bones[8];
    int count;
};

}

namespace {

constexpr int kRings = 48;
constexpr int kSides = 40;
constexpr float kPi = 3.14159265f;



void shape_torus(engine::Mesh &mesh, int tick) {
    mesh.vertices.resize(kRings * kSides);
    for (int ring = 0; ring < kRings; ring++) {
        float u = 2 * kPi * ring / kRings;
        float tube = 0.35f * (1 + 0.15f * std::sin(3 * u + 0.5f * tick));
        for (int side = 0; side < kSides; side++) {
            float v = 2 * kPi * side / kSides;
            engine::Vec3 normal{std::cos(u) * std::cos(v), std::sin(v), std::sin(u) * std::cos(v)};
            engine::Vertex &vertex = mesh.vertices[ring * kSides + side];
            vertex.position = {
                std::cos(u) + tube * normal.x,
                tube * normal.y,
                std::sin(u) + tube * normal.z,
            };
            vertex.normal = normal;
            vertex.color[0] = static_cast<std::uint8_t>(80 + 160 * ring / kRings);
            vertex.color[1] = 140;
            vertex.color[2] = static_cast<std::uint8_t>(220 - 160 * ring / kRings);
            vertex.color[3] = 255;
        }
    }
    if (!mesh.indices.empty()) {
        return;
    }
    for (int ring = 0; ring < kRings; ring++) {
        for (int side = 0; side < kSides; side++) {
            auto at = [](int r, int s) {
                return static_cast<std::uint32_t>((r % kRings) * kSides + (s % kSides));
            };
            std::uint32_t a = at(ring, side);
            std::uint32_t b = at(ring + 1, side);
            std::uint32_t c = at(ring + 1, side + 1);
            std::uint32_t d = at(ring, side + 1);
            mesh.indices.insert(mesh.indices.end(), {a, c, b, a, d, c});
        }
    }
}


engine::Mesh square() {
    engine::Mesh mesh;
    const float corners[4][2] = {{0, 0}, {1, 0}, {1, 1}, {0, 1}};
    for (const auto &corner : corners) {
        mesh.vertices.push_back({{corner[0], corner[1], 0}, {0, 0, 1}, {200, 200, 200, 255}});
    }
    mesh.indices = {0, 1, 2, 0, 2, 3};
    return mesh;
}

void pose(engine::Skeleton &skeleton, int tick) {
    float swing = 0.4f * std::sin(0.6f * tick);
    skeleton.count = 7;
    skeleton.bones[0] = {"hips", -1, 0, 0};
    skeleton.bones[1] = {"spine", 0, 0, 1.2f};
    skeleton.bones[2] = {"head", 1, 0, 1.8f};
    skeleton.bones[3] = {"arm.l", 1, -0.9f, 1.0f + swing};
    skeleton.bones[4] = {"arm.r", 1, 0.9f, 1.0f - swing};
    skeleton.bones[5] = {"leg.l", 0, -0.4f + swing, -1.4f};
    skeleton.bones[6] = {"leg.r", 0, 0.4f - swing, -1.4f};
}

std::size_t submitted = 0;


void submit(const engine::Mesh &mesh, const engine::Mesh &flown, const engine::Mesh &stray,
            const engine::Skeleton &skeleton, int tick) {
    submitted += mesh.indices.size() / 3 + flown.vertices.size() + stray.indices.size() +
                 static_cast<std::size_t>(skeleton.count);
    std::printf("tick %d: %zu\n", tick, submitted);
}

}

int main() {
    engine::Mesh torus;
    engine::Mesh flown = square();
    flown.vertices[2].position.x = std::numeric_limits<float>::quiet_NaN();
    engine::Mesh stray = square();
    stray.indices[4] = 7;
    engine::Skeleton skeleton{};
    for (int tick = 0; tick < 20; tick++) {
        shape_torus(torus, tick);
        pose(skeleton, tick);
        submit(torus, flown, stray, skeleton, tick);
    }
    return 0;
}
