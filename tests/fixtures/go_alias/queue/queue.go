package queue

type Queue struct {
	items []int
	limit int
}

// Ring names Queue; its methods are Queue's.
type Ring = Queue

func (r *Ring) Push(value int) {
	if len(r.items) < r.limit {
		r.items = append(r.items, value)
	}
}

func (r *Ring) Items() []int {
	return r.items
}
