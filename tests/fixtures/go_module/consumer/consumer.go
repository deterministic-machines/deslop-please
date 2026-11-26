package consumer

import "example.com/gofixture/queue"

func Clear(q *queue.Queue) {
    q.Items = nil
}
