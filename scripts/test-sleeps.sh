#!/usr/bin/env bash












set -euo pipefail

pattern='(thread::sleep|time::sleep|sleep_until|time\.sleep|asyncio\.sleep|setTimeout|\busleep|\bnanosleep|time\.Sleep|\bsleep)\(|Atomics\.wait\(([^,()]*,){3}|\bsleep [0-9]'
deadline='deadline|DEADLINE|expired|elapsed\(\)|timeout|TIMEOUT|attempts|tries'

declare -A classes=()
declare -A matched=()
while read -r class file text; do
  [[ -n "$class" ]] || continue
  if [[ ! "$class" =~ ^(poll|deadline|workload|parked|settle|race)$ ]]; then
    echo "error: scripts/test-sleeps.sh lists $file under an unknown class: $class" >&2
    exit 1
  fi
  classes["$file"$'\t'"$text"]=$class
done <<'LIST'
poll     editors/vscode/test/run.sh                   sleep 0.1
deadline editors/vscode/test/uat.js                   const timer = setTimeout(
deadline editors/vscode/test/uat.js                   const timer = setTimeout(
deadline editors/vscode/test/uat.js                   timer = setTimeout(() => reject(new Error(`timed out waiting for ${description}`)), TIMEOUT);
poll     editors/vscode/test/uat.js                   const poll = () => (predicate() ? resolve() : setTimeout(poll, 20));
workload sdk/rust-probes/src/probes/tests.rs          std::thread::sleep(std::time::Duration::from_millis(1200));
poll     src/agent/recorder/tests.rs                  thread::sleep(Duration::from_millis(25));
settle   src/agent/recorder/tests.rs                  thread::sleep(Duration::from_millis(250));
workload src/agent/recorder/tests.rs                  thread::sleep(Duration::from_millis(200));
workload src/agent/recorder/tests.rs                  thread::sleep(Duration::from_millis(400));
workload src/agent/recorder/tests.rs                  thread::sleep(Duration::from_secs(22));
poll     src/backend/linux/cdp.rs                     std::thread::sleep(Duration::from_millis(1));
settle   src/backend/linux/perf/lease.rs              std::thread::sleep(RETRY * 3);
poll     src/backend/linux/perf/mod.rs                std::thread::sleep(Duration::from_millis(1));
workload src/backend/linux/perf/mod.rs                std::thread::sleep(Duration::from_millis(500));
settle   src/backend/linux/proc_status.rs             std::thread::sleep(std::time::Duration::from_millis(20));
settle   src/backend/linux/reaped.rs                  std::thread::sleep(Duration::from_millis(5));
settle   src/backend/linux/reaped.rs                  std::thread::sleep(Duration::from_millis(5));
settle   src/backend/linux/reaped.rs                  std::thread::sleep(Duration::from_millis(5));
deadline src/backend/linux/runtime_agents.rs          std::thread::sleep(Duration::from_secs(2));
poll     src/backend/linux/signal_action.rs           std::thread::sleep(Duration::from_millis(1));
poll     src/backend/linux/syscalls/launch.rs         std::thread::sleep(Duration::from_millis(5));
settle   src/backend/linux/tracer.rs                  thread::sleep(std::time::Duration::from_micros(200));
poll     src/profiler/collector.rs                    std::thread::sleep(std::time::Duration::from_millis(1));
settle   src/profiler/collector.rs                    time.sleep(0.3)
poll     src/profiler/session.rs                      std::thread::sleep(Duration::from_micros(100));
workload src/profiler/session.rs                      std::thread::sleep(Duration::from_millis(30));
parked   src/web/picker.rs                            .any(|process| process.pid == pid && process.command == "sleep 30")
parked   src/web/picker.rs                            assert_eq!(found.command, "sleep 30");
poll     tests/agent.rs                               std::thread::sleep(Duration::from_millis(50));
poll     tests/debugger/group.rs                      std::thread::sleep(Duration::from_millis(1));
poll     tests/debugger/group.rs                      tokio::time::sleep(Duration::from_millis(1)).await;
workload tests/fixtures/c/kvstore.c                   nanosleep(&pause, NULL);
parked   tests/fixtures/c/process-tree.c              snprintf(command, sizeof command, "sleep 100; true");
workload tests/fixtures/c/profile-offcpu.c            while (nanosleep(&nap, &nap) != 0) {
workload tests/fixtures/c/syscall-exceptions.c       nanosleep(&pause, NULL);
workload tests/fixtures/c/syscalls.c                 nanosleep(&two, NULL);
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
poll     tests/support/mod.rs                         tokio::time::sleep(Duration::from_millis(1)).await;
poll     tests/support/web.rs                         std::thread::sleep(Duration::from_millis(10));
poll     tests/syscalls.rs                            std::thread::sleep(Duration::from_millis(10));
workload tests/syscalls.rs                            std::thread::sleep(Duration::from_millis(200));
workload tests/syscalls.rs                            std::thread::sleep(Duration::from_millis(250));
poll     tests/tokio/soak.rs                          tokio::time::sleep(Duration::from_millis(5)).await;
poll     tests/web/terminal.rs                        std::thread::sleep(Duration::from_millis(10));
deadline web/e2e/server.ts                            const timer = setTimeout(
deadline web/e2e/services.ts                          const timer = setTimeout(() => reject(new Error(`mock-idp never listened: ${log}`)), 10_000);
deadline web/e2e/services.ts                          const timer = setTimeout(() => reject(new Error(`services never listened: ${log}`)), 10_000);
poll     web/e2e/services.ts                          setTimeout(wake, left);
settle   web/e2e/visualizers/hostile.js               setTimeout(() => resolve("pending"), ms);
workload web/test/live.test.tsx                       setTimeout(() => {
workload web/test/sandbox.test.tsx                    "microscope.draw(() => { setTimeout(() => { throw new Error('later'); }); return new Promise(() => {}); });",
workload web/test/sandbox.test.tsx                    "microscope.draw((input) => new Promise((resolve) => setTimeout(() => resolve(microscope.picture({ width: 1, height: 1, shapes: [] })), 300)));",
LIST

failed=0
while IFS= read -r hit; do
  file=${hit%%:*}
  rest=${hit#*:}
  line=${rest%%:*}
  text=${rest#*:}
  text="${text#"${text%%[![:space:]]*}"}"
  text="${text%"${text##*[![:space:]]}"}"

  [[ "$text" =~ ^(//|#|/\*|\*|--) || "$text" == *'asyncio.sleep(0)'* || "$text" =~ setTimeout\([a-z]+,\ 0\)\; || "$text" == 'test.setTimeout('* ]] && continue

  key="$file"$'\t'"$text"
  class=${classes[$key]:-}
  if [[ -z "$class" ]]; then
    echo "error: $file:$line sleeps where scripts/test-sleeps.sh does not class it: $text" >&2
    echo "  wait on what the test can observe, or list it as poll, deadline, workload, parked, settle or race" >&2
    failed=1
    continue
  fi
  matched[$key]=1

  if [[ "$class" == poll ]] && ! sed -n "$(( line > 15 ? line - 15 : 1 )),${line}p" "$file" | grep -qE "$deadline"; then
    echo "error: $file:$line polls without a deadline in the 15 lines above it: $text" >&2
    failed=1
  fi
done < <(
  git ls-files -z --cached --others --exclude-standard -- tests web/test web/e2e editors ':!:tests/fixtures/build/' \
    | xargs -0 grep -nHE "$pattern" || true

  git ls-files -z --cached --others --exclude-standard -- 'src/**.rs' 'crates/**.rs' 'sdk/**.rs' \
    | xargs -0 awk '
      FNR == 1 { whole = FILENAME ~ /(^|\/)tests(\/|\.rs$)/; armed = 0; depth = 0 }
      /^[[:space:]]*#\[cfg\(test\)\][[:space:]]*$/ { armed = 1; next }
      armed && /^[[:space:]]*(pub(\([a-z]+\))? )?mod [a-z_0-9]+ \{/ { armed = 0; depth = 1; next }
      { armed = 0 }
      depth > 0 {
        line = $0
        depth += gsub(/\{/, "", line) - gsub(/\}/, "", line)
        print FILENAME ":" FNR ":" $0
        next
      }
      whole { print FILENAME ":" FNR ":" $0 }
    ' | grep -E "^[^:]+:[0-9]+:.*($pattern)" || true
)

for key in "${!classes[@]}"; do
  if [[ -z "${matched[$key]:-}" ]]; then
    echo "error: scripts/test-sleeps.sh lists a sleep no longer there: ${key/$'\t'/: }" >&2
    failed=1
  fi
done

exit "$failed"
