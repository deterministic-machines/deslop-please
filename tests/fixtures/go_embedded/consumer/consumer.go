package consumer

import "example.com/embedded/queue"

func Reset(q *queue.Queue) {
	q.N = 0
}
