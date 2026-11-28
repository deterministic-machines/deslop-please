from app.queue import RunQueue

def push_bad(queue: RunQueue, value: int) -> None:
    queue.contents.append(value)
