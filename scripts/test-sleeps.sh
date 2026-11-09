#!/usr/bin/env bash
# Fails when a test or fixture sleeps where the list below does not class
# it. A test that waits a while and then acts on what it hopes has happened
# breaks under load (dev/testing.md): it should wait on what it can observe.
# Each sleep left is one of:
#
#   poll      inside a loop that fails at a deadline, which must be named
#             within the 15 lines above it
#   workload  the program's job is to run, or pace itself, that long
#   parked    the sleep is the state the test reads
#
# An entry is a sleep's file and its line's text, trimmed, so it outlives
# edits to the lines around it. An entry that matches nothing fails too.
set -euo pipefail

pattern='(thread::sleep|time::sleep|sleep_until|time\.sleep|asyncio\.sleep|setTimeout|usleep|nanosleep|time\.Sleep|\bsleep)\(|Atomics\.wait\(([^,()]*,){3}|\bsleep [0-9]'
deadline='deadline|DEADLINE|expired|elapsed\(\)|timeout|TIMEOUT|attempts|tries'

declare -A classes=()
declare -A matched=()
while read -r class file text; do
  [[ -n "$class" ]] || continue
  classes["$file"$'\t'"$text"]=$class
done <<'LIST'
poll     tests/breakpoint_states.rs                   tokio::time::sleep(Duration::from_millis(1)).await;
parked   tests/debugger/attach.rs                     std::process::Command::new("sh").args(["-c", "sleep 30"]),
poll     tests/debugger/forks.rs                      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
poll     tests/debugger/group.rs                      std::thread::sleep(Duration::from_millis(1));
poll     tests/debugger/group.rs                      tokio::time::sleep(Duration::from_millis(10)).await;
poll     tests/debugger/group.rs                      tokio::time::sleep(Duration::from_millis(1)).await;
workload tests/fixtures/c/kvstore.c                   nanosleep(&pause, NULL);
parked   tests/fixtures/c/process-tree.c              snprintf(command, sizeof command, "sleep 100; true");
workload tests/fixtures/c/profile-offcpu.c            while (nanosleep(&nap, &nap) != 0) {
poll     tests/fixtures/c/thread-exec.c               nanosleep(&wait, NULL);
workload tests/fixtures/go/kvstore/main.go            time.Sleep(20 * time.Millisecond)
parked   tests/fixtures/go/workers/main.go            time.Sleep(time.Hour)
workload tests/fixtures/node/fast.cc                  usleep(static_cast<useconds_t>(info[0].As<v8::Int32>()->Value()));
workload tests/fixtures/node/fast.cc                  usleep(static_cast<useconds_t>(microseconds));
workload tests/fixtures/node/frameless.js             setTimeout(tick, 10);
workload tests/fixtures/node/live-stress.js           const name = first('mixed'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS + 0 * `${value}`.length); else Atomics.wait(cell, 0, 0, BLOCK_MS + 0 * `${value}`.length);
workload tests/fixtures/node/live-stress.js           const name = first('nested'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
workload tests/fixtures/node/live-stress.js           const name = first('reviver'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
workload tests/fixtures/node/live-stress.js           const name = first('sort'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
workload tests/fixtures/node/live-stress.js           const name = first('wait'); if (name) truth.blocking(name), Atomics.wait(cell, 0, 0, BLOCK_MS); else Atomics.wait(cell, 0, 0, BLOCK_MS);
parked   tests/fixtures/node/tasks.js                 const idle = named('idleTimer', () => setTimeout(function idleTimer() {}, 60000).unref());
parked   tests/fixtures/node/tasks.js                 const timeout = named('sessionTimeout', () => setTimeout(function sessionTimeout() {}, 30000));
parked   tests/fixtures/node/tasks.js                 const timer = setTimeout(function workerTimer() {}, 40000);
poll     tests/fixtures/node/truth.js                 if (Atomics.wait(worker.words, PARKED, 0, deadline - Date.now()) === 'timed-out') {
workload tests/fixtures/node/waits.js                 Atomics.wait(cell, 0, 0, 2);
poll     tests/fixtures/python/agent_safety.py        time.sleep(0.001)
parked   tests/fixtures/python/bootstrap.py           time.sleep(0.01)
workload tests/fixtures/python/profiled.py            time.sleep(0.001)  # LINE nap
parked   tests/fixtures/python/tasks.py               await asyncio.sleep(3600)
poll     tests/fixtures/python/tasks.py               time.sleep(0.001)
poll     tests/fixtures/python/truth.py               time.sleep(0.001)
workload tests/fixtures/rust/kvstore.rs               thread::sleep(pause);
poll     tests/fixtures/rust/probes/main.rs           std::thread::sleep(Duration::from_millis(1));
workload tests/fixtures/rust/probes/main.rs           std::thread::sleep(Duration::from_millis(30));
parked   tests/fixtures/rust/tokio/runtimes/src/main.rs async fn sleep() {
parked   tests/fixtures/rust/tokio/runtimes/src/main.rs multi.spawn(parked("multi", "sleep", sleep()));
parked   tests/fixtures/rust/tokio/runtimes/src/main.rs tokio::spawn(parked("current", "sleep", sleep()));
parked   tests/fixtures/rust/tokio/runtimes/src/main.rs tokio::task::spawn_local(parked("local", "sleep", sleep()));
parked   tests/fixtures/rust/tokio/runtimes/src/main.rs tokio::task::spawn_local(parked("shared", "sleep", sleep()));
parked   tests/fixtures/rust/tokio/runtimes/src/main.rs tokio::time::sleep(Duration::from_secs(3600)).await;
parked   tests/fixtures/rust/tokio/values/src/main.rs let elapsed = tokio::time::sleep_until(early);
parked   tests/fixtures/rust/tokio/values/src/main.rs let mut registered = Box::pin(tokio::time::sleep(Duration::from_secs(100)));
parked   tests/fixtures/rust/tokio/values/src/main.rs let sleep = tokio::time::sleep(Duration::from_secs(90));
parked   tests/fixtures/rust/tokio/values/src/main.rs tokio::time::sleep(Duration::from_secs(80))
parked   tests/fixtures/rust/tokio/workers/src/main.rs tokio::time::sleep(Duration::from_secs(3600)).await; // AWAIT: sleep
poll     tests/hit_counts.rs                          tokio::time::sleep(Duration::from_millis(1)).await;
poll     tests/node/attach.rs                         tokio::time::sleep(std::time::Duration::from_millis(20)).await;
poll     tests/node/debian.rs                         std::thread::sleep(Duration::from_millis(10));
poll     tests/node/debian.rs                         tokio::time::sleep(Duration::from_millis(20)).await;
poll     tests/node/functions.rs                      tokio::time::sleep(std::time::Duration::from_millis(20)).await;
poll     tests/node/inspector.rs                      tokio::time::sleep(Duration::from_millis(10)).await;
poll     tests/node/inspector.rs                      tokio::time::sleep(Duration::from_millis(5)).await;
poll     tests/node/session.rs                        tokio::time::sleep(Duration::from_millis(10)).await;
poll     tests/node/session.rs                        tokio::time::sleep(Duration::from_millis(20)).await;
poll     tests/node/session.rs                        tokio::time::sleep(Duration::from_millis(5)).await;
poll     tests/python/agent.rs                        std::thread::sleep(Duration::from_millis(10));
poll     tests/python/profile.rs                      std::thread::sleep(Duration::from_millis(10));
poll     tests/python/profile.rs                      time.sleep(0.001)
poll     tests/python/session.rs                      tokio::time::sleep(Duration::from_millis(10)).await;
poll     tests/python/session.rs                      tokio::time::sleep(Duration::from_millis(20)).await;
poll     tests/support/mod.rs                         std::thread::sleep(Duration::from_millis(1));
poll     tests/support/web.rs                         std::thread::sleep(Duration::from_millis(10));
poll     tests/syscalls.rs                            std::thread::sleep(Duration::from_millis(10));
poll     tests/tokio/soak.rs                          tokio::time::sleep(Duration::from_millis(5)).await;
poll     tests/watch_conditions.rs                    tokio::time::sleep(Duration::from_millis(1)).await;
poll     tests/web/terminal.rs                        std::thread::sleep(Duration::from_millis(10));
LIST

failed=0
while IFS= read -r hit; do
  file=${hit%%:*}
  rest=${hit#*:}
  line=${rest%%:*}
  text=${rest#*:}
  text="${text#"${text%%[![:space:]]*}"}"
  text="${text%"${text##*[![:space:]]}"}"
  # Comments, and yielding to the event loop, sleep nothing.
  [[ "$text" =~ ^(//|#|/\*|\*) || "$text" == *'asyncio.sleep(0)'* ]] && continue

  key="$file"$'\t'"$text"
  class=${classes[$key]:-}
  if [[ -z "$class" ]]; then
    echo "error: $file:$line sleeps where scripts/test-sleeps.sh does not class it: $text" >&2
    echo "  wait on what the test can observe, or list it as poll, workload or parked" >&2
    failed=1
    continue
  fi
  matched[$key]=1

  if [[ "$class" == poll ]] && ! sed -n "$(( line > 15 ? line - 15 : 1 )),${line}p" "$file" | grep -qE "$deadline"; then
    echo "error: $file:$line polls without a deadline in the 15 lines above it: $text" >&2
    failed=1
  fi
done < <(git ls-files -z --cached --others --exclude-standard -- tests ':!:tests/fixtures/build/' \
  | xargs -0 grep -nHE "$pattern" || true)

for key in "${!classes[@]}"; do
  if [[ -z "${matched[$key]:-}" ]]; then
    echo "error: scripts/test-sleeps.sh lists a sleep no longer there: ${key/$'\t'/: }" >&2
    failed=1
  fi
done

exit "$failed"
