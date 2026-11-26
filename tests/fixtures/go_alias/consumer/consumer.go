package consumer

import "example.com/alias/queue"

type Local = queue.Ring

func ReplaceFirst(r *Local) {
	r.Items()[0] = 42
}
