package queue

type Counter struct {
	N int
}

type Queue struct {
	Counter
	items []int
}

func (q *Queue) Push(value int) {
	q.items = append(q.items, value)
	q.N++
}
