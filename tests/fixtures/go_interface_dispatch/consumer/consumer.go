package consumer

import "example.com/interfacedispatch/queue"

type ItemsProvider interface {
    Items() []int
}

func ReplaceFirst(p ItemsProvider) {
    p.Items()[0] = 42
}

func ReplaceQueue(q *queue.Queue) {
    ReplaceFirst(q)
}
