package app

import "example.com/demo/queue"

func resetInTest(q *queue.Queue) {
	q.Items = nil
}
