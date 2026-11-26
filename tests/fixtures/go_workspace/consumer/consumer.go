package consumer

import "example.com/owner/queue"

func Clear(q *queue.Queue) {
    q.Items = nil
}
