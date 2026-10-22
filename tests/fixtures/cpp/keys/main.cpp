// Maps keyed by pairs and tuples, which an expression indexes by a tuple
// key. Prints `EXPECT\t<expression>\t<kind>\t<value>` for each, as the
// map's own lookup finds it, then calls barrier().

#include <cstdio>
#include <map>
#include <string>
#include <tuple>
#include <utility>

extern "C" void barrier(void *fixture);

struct Point {
    int x;
    int y;
    bool operator<(const Point &other) const {
        return std::tie(x, y) < std::tie(other.x, other.y);
    }
};

static void integer(const char *expression, long long value) {
    std::printf("EXPECT\t%s\tint\t%lld\n", expression, value);
}

static void error(const char *expression, const char *kind) {
    std::printf("EXPECT\t%s\terror\t%s\n", expression, kind);
}

int main() {
    std::map<std::pair<int, int>, int> grid{{{1, 2}, 12}, {{2, 3}, 23}, {{-1, 7}, -17}};
    std::map<std::pair<std::string, unsigned char>, unsigned> names{{{"ann", 3}, 7}, {{"bob", 1}, 9}};
    std::map<std::tuple<int, std::string, long>, int> triple{{{1, "x", 3}, 6}, {{1, "y", 3}, 4}};
    std::map<std::pair<int, std::pair<long, bool>>, int> nested{{{1, {2, true}}, 5}, {{1, {2, false}}, 6}};
    std::map<std::tuple<int>, int> single{{{4}, 40}};
    std::map<Point, int> points{{{1, 2}, 3}};
    std::pair<int, int> cell{2, 3};
    std::pair<long, bool> inner{2, false};

    integer("grid[1, 2]", grid.at({1, 2}));
    integer("grid[(2, 3)]", grid.at({2, 3}));
    integer("grid[-1, 7]", grid.at({-1, 7}));
    integer("grid[cell]", grid.at(cell));
    integer("grid[1, 2] * 2", grid.at({1, 2}) * 2);
    integer("names[\"ann\", 3]", names.at({"ann", 3}));
    integer("names[(\"bob\", 1)]", names.at({"bob", 1}));
    integer("triple[1, \"y\", 3]", triple.at({1, "y", 3}));
    integer("nested[1, (2, true)]", nested.at({1, {2, true}}));
    integer("nested[1, inner]", nested.at({1, inner}));
    integer("single[4,]", single.at(std::tuple<int>{4}));
    if (grid.count({3, 3}) == 0) error("grid[3, 3]", "missing-key");
    if (names.count({"ann", 4}) == 0) error("names[\"ann\", 4]", "missing-key");
    if (triple.count({1, "z", 3}) == 0) error("triple[1, \"z\", 3]", "missing-key");
    error("grid[1]", "type");
    error("grid[1, 2, 3]", "type");
    error("nested[1, 2, true]", "type");
    error("points[1, 2]", "type");
    std::fflush(stdout);

    barrier(&grid);
    void *volatile kept[] = {&grid, &names, &triple, &nested, &single, &points, &cell, &inner};
    (void)kept;
    return 0;
}
