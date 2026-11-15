





#include <stdio.h>

extern long Triple(long value);

static _Thread_local long calls[8];

int main(void) {

	char line[8];
	if (fgets(line, sizeof line, stdin) == NULL && ferror(stdin)) {
		return 1;
	}

	calls[1] += 1;
	long tripled = Triple(14); // HOST: call
	printf("%ld %ld\n", tripled, calls[1]);
	return 0;
}
