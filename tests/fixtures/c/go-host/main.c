// Hosts a Go library, calling into it from its own thread. The host's own
// thread-local storage comes first in each thread's block, so the
// library's lies further from the thread pointer than the runtime would
// put it in a program of its own. It calls once a line arrives on its
// input, or the input ends, so a recording can find the library's
// goroutine waiting first.
#include <stdio.h>

extern long Triple(long value);

static _Thread_local long calls[8];

int main(void) {
	// The input's end, as when it is empty, calls at once.
	char line[8];
	if (fgets(line, sizeof line, stdin) == NULL && ferror(stdin)) {
		return 1;
	}

	calls[1] += 1;
	long tripled = Triple(14); // HOST: call
	printf("%ld %ld\n", tripled, calls[1]);
	return 0;
}
