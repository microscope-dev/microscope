


#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define ROWS 48
#define COLUMNS 64

struct life {
    int generation;
    uint8_t cells[ROWS][COLUMNS];
};


static void seed(struct life *life) {
    memset(life, 0, sizeof *life);
    static const int glider[][2] = {{1, 2}, {2, 3}, {3, 1}, {3, 2}, {3, 3}};
    for (size_t i = 0; i < sizeof glider / sizeof glider[0]; i++) {
        life->cells[glider[i][0]][glider[i][1]] = 1;
    }
    for (int column = 30; column < 33; column++) {
        life->cells[20][column] = 1;
    }
}

static int neighbours(const struct life *life, int row, int column) {
    int count = 0;
    for (int dr = -1; dr <= 1; dr++) {
        for (int dc = -1; dc <= 1; dc++) {
            if (dr != 0 || dc != 0) {
                count += life->cells[(row + dr + ROWS) % ROWS][(column + dc + COLUMNS) % COLUMNS];
            }
        }
    }
    return count;
}

static void step(struct life *life) {
    static uint8_t next[ROWS][COLUMNS];
    for (int row = 0; row < ROWS; row++) {
        for (int column = 0; column < COLUMNS; column++) {
            int around = neighbours(life, row, column);
            next[row][column] = around == 3 || (around == 2 && life->cells[row][column]);
        }
    }
    memcpy(life->cells, next, sizeof next);
    life->generation++;
}

int main(int argc, char **argv) {
    static struct life life;
    int generations = argc > 1 ? atoi(argv[1]) : 100;
    seed(&life);
    for (int i = 0; i < generations; i++) {
        step(&life);
        printf("generation %d\n", life.generation);
    }
    return 0;
}
