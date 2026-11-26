//go:build expose

package consumer

import "example.com/buildtags/queue"

func Clear(q *queue.Queue) {
    q.Items = nil
}
