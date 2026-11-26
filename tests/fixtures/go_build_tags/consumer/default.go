//go:build !expose

package consumer

import "example.com/buildtags/queue"

func Add(q *queue.Queue) {
    q.Push(1)
}
