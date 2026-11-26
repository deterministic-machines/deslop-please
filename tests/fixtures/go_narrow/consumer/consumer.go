package consumer

import "example.com/narrow/queue"

func Add(q *queue.Queue, value int) {
    q.Push(value)
}
