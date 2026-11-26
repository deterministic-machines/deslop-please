package consumer

import "example.com/gettervalue/queue"

func Count(q *queue.Queue) int {
    return q.Count()
}
