



#include <cstdio>
#include <map>
#include <string>

struct Node {
    int key;

    int left;
    int right;
};

struct Tree {
    Node pool[64];
    int count = 0;
    int root = -1;

    void insert(int key) {
        int added = count++;
        pool[added] = Node{key, -1, -1};
        if (root < 0) {
            root = added;
            return;
        }
        int at = root;
        for (;;) {
            int &next = key < pool[at].key ? pool[at].left : pool[at].right;
            if (next < 0) {
                next = added;
                return;
            }
            at = next;
        }
    }
};

int main() {
    Tree tree;
    std::map<std::string, int> counts;
    const int keys[] = {50, 30, 70, 20, 40, 60, 80, 35, 45, 65, 10, 90, 85, 25};
    const char *words[] = {"tree", "map", "node", "tree", "key", "tree", "map"};
    for (int i = 0; i < 14; i++) {
        tree.insert(keys[i]);
        counts[words[i % 7]] += 1;
        std::printf("inserted %d: %d nodes, %zu words\n", keys[i], tree.count, counts.size());
    }
    return 0;
}
