package consumer

import "example.com/mixed/gosvc"

func Push(queue *gosvc.Queue, item gosvc.Item) {
	queue.Items = append(queue.Items, item)
}

func Drain(queue *gosvc.Queue) []gosvc.Item {
	return queue.RawItems()
}
