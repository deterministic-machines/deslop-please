package consumer

import "example.com/reads/queue"

func Summary(q *queue.Queue) (int, int, string) {
	total := 0
	for _, item := range q.Items {
		total += item
	}
	return len(q.Items), total + q.Items[0], q.Name
}
