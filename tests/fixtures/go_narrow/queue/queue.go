package queue

type Queue struct {
    items []int
    limit int
}

func (q *Queue) Push(value int) {
    if len(q.items) < q.limit {
        q.items = append(q.items, value)
    }
}

func (q *Queue) Len() int {
    return len(q.items)
}
