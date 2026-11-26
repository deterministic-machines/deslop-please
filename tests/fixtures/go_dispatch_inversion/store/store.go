package store

// Logger is how a Store reports, without knowing who listens.
type Logger interface {
	Log(message string)
}

type Store struct {
	log   Logger
	count int
}

func New(log Logger) *Store {
	return &Store{log: log}
}

func (s *Store) Put() {
	s.count++
	s.log.Log("put")
}

func (s *Store) Count() int {
	return s.count
}
