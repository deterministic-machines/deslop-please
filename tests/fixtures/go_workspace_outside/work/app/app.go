package app

import "example.com/shared/queue"

func Clear(q *queue.Queue) {
	q.Items = nil
}
