package app

import "example.com/demo/queue"

func Push(q *queue.Queue, item queue.Item) {
	q.Items = append(q.Items, item)
}

func Drain(q *queue.Queue) []queue.Item {
	return q.RawItems()
}
