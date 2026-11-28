from app.queue import RunQueue

def test_items(queue: RunQueue) -> None:
    queue.items.append(1)
