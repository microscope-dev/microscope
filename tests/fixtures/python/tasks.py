





















import asyncio
import os
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402

RECORDS = []


class Tagged(asyncio.Task):
    """A task of a class deriving from asyncio's."""


async def leaf(event):
    deadline = 3600
    await event.wait()
    return deadline


async def middle(event):
    depth = 2
    await leaf(event)
    return depth


async def sleeper():
    await asyncio.sleep(3600)


async def waiter(task):
    await task


async def noop():
    pass


def describe(task, running):

    graph = asyncio.capture_call_graph(task)
    awaiters = [entry.future.get_name() for entry in graph.awaited_by]
    if task is running:
        state, detail = "running", "-"
    elif task.done():
        state, detail = "exited", "-"
    elif task._fut_waiter is not None:
        state = "blocked"
        waiter = task._fut_waiter
        if isinstance(waiter, asyncio.Task):
            detail = f"awaits task {waiter.get_name()}"
        else:
            detail = f"awaits {type(waiter).__module__}.{type(waiter).__qualname__}"
    else:
        state, detail = "runnable", "-"
    return state, detail, ", ".join(awaiters) or "-"


def chain(task):

    frames = []
    coroutine = task.get_coro()
    while coroutine is not None and hasattr(coroutine, "cr_frame"):
        frame = coroutine.cr_frame
        if frame is None:
            break
        frames.append(frame)
        coroutine = coroutine.cr_await
    return list(reversed(frames))


def report(tasks, names, running):
    for task in tasks:
        name = names.get(task) or task.get_name()
        state, detail, awaiters = describe(task, running)
        RECORDS.append(("task", name, state, detail, awaiters))
        if task is running:
            continue
        for depth, frame in enumerate(chain(task)):
            code = frame.f_code
            RECORDS.append(
                ("task-frame", name, depth, code.co_qualname, code.co_filename, frame.f_lineno)
            )
            for local, value in frame.f_locals.items():
                RECORDS.append(("task-local", name, depth, local, truth.canonical(value)))


def other_loop(started, done, loops):



    async def own():
        loops.append(asyncio.get_running_loop())
        current = asyncio.current_task()
        current.set_name("own")
        task = asyncio.create_task(sleeper(), name="threaded")
        await asyncio.sleep(0)
        started.append(current)
        await asyncio.get_running_loop().run_in_executor(None, done.wait)
        task.cancel()

    asyncio.run(own())


def blocked(started):

    return bool(started) and started[0]._fut_waiter is not None


async def main():
    event = asyncio.Event()
    chained = asyncio.create_task(middle(event), name="chained")
    probe = asyncio.create_task(noop())
    await probe
    number = int(probe.get_name().removeprefix("Task-"))
    unnamed = asyncio.create_task(sleeper())
    awaiting = asyncio.create_task(waiter(chained), name="awaiting")
    shared = asyncio.create_task(sleeper(), name="shared")
    first = asyncio.create_task(waiter(shared), name="first")
    second = asyncio.create_task(waiter(shared), name="second")
    tagged = Tagged(sleeper(), name="tagged")
    await asyncio.sleep(0)
    await asyncio.sleep(0)
    fresh = asyncio.create_task(sleeper(), name="fresh")

    started = []
    done = threading.Event()
    loops = []
    thread = threading.Thread(target=other_loop, args=(started, done, loops), name="loop")
    thread.start()
    deadline = time.monotonic() + 30
    while not blocked(started):
        if time.monotonic() > deadline:
            raise TimeoutError("the other loop's task never waited")
        time.sleep(0.001)

    running = asyncio.current_task()
    running.set_name("main")
    names = {unnamed: f"Task-{number + 1}"}
    report(asyncio.all_tasks(), names, running)
    report(asyncio.all_tasks(loops[0]), names, running)
    del chained, awaiting, first, second, tagged, fresh
    truth.checkpoint("parked", records=RECORDS)
    done.set()
    thread.join()


asyncio.run(main())
