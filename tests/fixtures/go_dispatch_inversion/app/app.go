package app

import "example.com/inversion/store"

type Audit struct {
	store *store.Store
	lines int
}

func (a *Audit) Log(message string) {
	a.lines += a.store.Count() + len(message)
}

func (a *Audit) Attach(s *store.Store) {
	a.store = s
	a.lines = 0
}

func (a *Audit) Record() int {
	a.store.Put()
	return a.lines
}

func Run() int {
	audit := &Audit{}
	audit.Attach(store.New(audit))
	return audit.Record()
}
