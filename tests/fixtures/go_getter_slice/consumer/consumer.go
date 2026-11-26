package consumer

import "example.com/getterslice/queue"

func ReplaceFirst(q *queue.Queue) {
    q.Items()[0] = 42
}
