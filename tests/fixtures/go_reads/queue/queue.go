package queue

type Queue struct {
	Items []int
	Name  string
	limit int
}

func (q *Queue) Push(value int) {
	if len(q.Items) < q.limit {
		q.Items = append(q.Items, value)
	}
}
