package queue

type Queue struct {
    count int
    limit int
}

func (q *Queue) Push() {
    if q.count < q.limit {
        q.count++
    }
}

func (q *Queue) Count() int {
    return q.count
}
