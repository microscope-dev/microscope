import { describe, expect, it } from "vitest";

import { blockedIn, kernelLines, sliceAt, stepAt, waitAt } from "../src/profile/kernel";
import type { ProfileLane } from "../src/protocol";

const lane = (change: Partial<ProfileLane> = {}): ProfileLane => ({
  thread: 0,
  task: null,
  weights: [],
  blocked: [],
  preempted: [],
  syscalls: [0, 3, 0, 1],
  failed: [0, 1, 0, 0],
  waits: [],
  ...change,
});

describe("a kernel lane under the pointer", () => {
  it("finds the slice a time falls in, the last one ending the range", () => {
    expect(sliceAt([100, 200], 4, 100)).toBe(0);
    expect(sliceAt([100, 200], 4, 124)).toBe(0);
    expect(sliceAt([100, 200], 4, 125)).toBe(1);
    expect(sliceAt([100, 200], 4, 199)).toBe(3);
    expect(sliceAt([100, 200], 4, 200)).toBe(-1);
    expect(sliceAt([100, 200], 4, 99)).toBe(-1);
    expect(sliceAt([100, 200], 0, 150)).toBe(-1);
  });

  it("names the call a thread waited in, nearest first, within the slack", () => {
    const waits = [
      { call: "futex", start: 10, end: 20 },
      { call: "read", start: 26, end: 40 },
    ];
    expect(waitAt(lane({ waits }), 15, 0)?.call).toBe("futex");

    expect(waitAt(lane({ waits }), 24, 3)?.call).toBe("read");
    expect(waitAt(lane({ waits }), 22, 3)?.call).toBe("futex");
    expect(waitAt(lane({ waits }), 23, 2)).toBeNull();
  });

  it("says what waited, or how many calls returned and failed, or nothing", () => {
    const waits = [{ call: "futex", start: 0, end: 2_000_000 }];
    expect(kernelLines(lane({ waits }), [0, 4000], 1000, 1)?.[0]).toBe(
      "waited in futex for 2.00 ms",
    );
    expect(kernelLines(lane(), [0, 4000], 1500, 1)).toEqual(["3 system calls, 1 failed"]);
    expect(kernelLines(lane(), [0, 4000], 3500, 1)).toEqual(["1 system call"]);
    expect(kernelLines(lane(), [0, 4000], 2500, 1)).toBeNull();
  });

  it("finds the budget's step nearest the pointer", () => {
    const steps = [
      { time: 100, text: "sampling at 499 Hz" },
      { time: 110, text: "sampling at 249 Hz" },
    ];
    expect(stepAt(steps, 104, 10)?.text).toBe("sampling at 499 Hz");
    expect(stepAt(steps, 107, 10)?.text).toBe("sampling at 249 Hz");
    expect(stepAt(steps, 50, 10)).toBeNull();
  });
});

describe("an off-CPU span's blocking calls", () => {
  const waited = lane({
    waits: [
      { call: "read", start: 0, end: 10 },
      { call: "futex", start: 20, end: 60 },
      { call: "read", start: 70, end: 90 },
      { call: "poll", start: 200, end: 300 },
    ],
  });

  it("names each call it overlaps, by the time it spent there, longest first", () => {
    expect(blockedIn(waited, 5, 80)).toBe("blocked in futex 40 ns, read ×2 15 ns");
  });

  it("names none where it waited in none, nor at a wait's edges", () => {
    expect(blockedIn(waited, 100, 200)).toBeNull();
    expect(blockedIn(waited, 10, 20)).toBeNull();
  });
});
