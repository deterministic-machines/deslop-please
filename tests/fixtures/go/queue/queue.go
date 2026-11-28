package queue

type Item struct {
	ID int
}

type Queue struct {
	Items []Item
	limit int
}

func NewQueue(limit int) *Queue {
	return &Queue{limit: limit}
}

func (q *Queue) Full() bool {
	return len(q.Items) >= q.limit
}

func (q *Queue) RawItems() []Item {
	return q.Items
}
