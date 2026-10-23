#include <array>
#include <map>
#include <memory>
#include <optional>
#include <string>
#include <variant>
#include <vector>

struct Record {
    std::string name;
    std::vector<int> samples;
    std::optional<double> weight;
};

int main() {
    std::vector<Record> records = {
        {"alpha", {2, 3, 5}, 1.5},
        {"beta", {7, 11}, std::nullopt},
    };
    std::map<std::string, std::variant<int, std::string>> labels = {
        {"count", 2}, {"unit", std::string("samples")},
    };
    auto selected = std::make_unique<Record>(records.front());
    std::array<int, 3> bounds = {0, 10, 20};
    return records.size() != 2 || labels.size() != 2 ||
           selected->samples[1] != 3 || bounds[2] != 20;
}
