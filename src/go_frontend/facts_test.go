package main

import (
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

// Analyzes one package of sources, by file name, and returns its facts by
// file name.
func analyzeSources(t *testing.T, sources map[string]string) map[string]File {
	t.Helper()
	dir := t.TempDir()
	dir, err := filepath.EvalSymlinks(dir)
	if err != nil {
		t.Fatal(err)
	}
	var files []string
	for name, source := range sources {
		path := filepath.Join(dir, name)
		if err := os.WriteFile(path, []byte(source), 0o644); err != nil {
			t.Fatal(err)
		}
		files = append(files, path)
	}
	sort.Strings(files)
	result := map[string]File{}
	input := Input{Dir: dir, Packages: []InputPackage{{Dir: dir, ImportPath: "example.com/p", Files: files}}}
	for _, file := range analyze(input) {
		result[filepath.Base(file.Path)] = file
	}
	return result
}

const queue = `package p

type Q struct {
	n    int
	s    []int
	m    map[string]int
	p    *Q
	a    [2]int
	name string
	f    func()
	ch   chan int
	in   Inner
	ps   []*Q
	Inner
}

type Inner struct{ k int }

func (q *Q) Ptr()        {}
func (i Inner) Get() int { return i.k }

type Num int

func (n Num) Twice() Num { return n * 2 }

func sink(...any) {}
`

// Each line of the function ends with the modes, in order, of the uses on
// it of fields and of what called functions return.
const cases = `package p

func cases(q *Q, x []int, y []*Q) {
	_ = q.n // want read
	q.n = 1 // want mutate
	q.n++ // want mutate
	q.n += 2 // want mutate
	_ = q.n == 1 // want read
	_ = len(q.s) // want read
	_ = q.s == nil // want read
	_ = q.s[0] // want read
	q.s[0] = 1 // want mutate
	x = q.s // want mutate
	q.s = append(q.s, 1) // want mutate mutate
	x = append(x, q.s...) // want read
	y = append(y, q.ps...) // want mutate
	copy(q.s, x) // want mutate
	copy(x, q.s) // want read
	_ = q.m["k"] // want read
	q.m["k"] = 1 // want mutate
	delete(q.m, "k") // want mutate
	clear(q.m) // want mutate
	_ = &q.n // want mutate
	_ = q.p.n // want read read
	q.p.n = 1 // want mutate mutate
	q.p.Ptr() // want mutate read
	_ = q.in.Get() // want read read
	_ = q.a[1] // want read
	x = q.a[:] // want mutate
	_ = q.name[1:] // want read
	_ = []byte(q.name) // want read
	_ = string(q.s[0]) // want read
	q.f() // want mutate
	q.ch <- 1 // want mutate
	_ = <-q.ch // want mutate
	for _, v := range q.s { _ = v } // want read
	for _, v := range q.ps { _ = v } // want mutate
	for i := range q.ps { _ = i } // want read
	for range q.ch {} // want mutate
	sink(q.n) // want read read
	sink(q.s) // want read mutate
	_ = q.k // want read
	_ = Num(q.n).Twice() // want read read
	_ = Q{n: 1, s: x} // want read read
	_ = Inner{1} // want
	y[0] = q.p // want mutate
	_ = any(q.n) // want read
	_ = any(q.s) // want mutate
}

func ret(q *Q) []int { return q.s } // want mutate

func ret2(q *Q) int { return q.n } // want read
`

func TestUsesSayWhetherCodeOnlyReads(t *testing.T) {
	facts := analyzeSources(t, map[string]string{"q.go": queue, "cases.go": cases})
	file := facts["cases.go"]
	byLine := map[int][]Use{}
	for _, use := range file.Uses {
		byLine[use.Pos.Line] = append(byLine[use.Pos.Line], use)
	}
	for i, line := range strings.Split(cases, "\n") {
		_, want, ok := strings.Cut(line, "// want")
		if !ok {
			continue
		}
		uses := byLine[i]
		sort.Slice(uses, func(a, b int) bool { return uses[a].Pos.Col < uses[b].Pos.Col })
		var got []string
		for _, use := range uses {
			got = append(got, use.Mode)
		}
		if strings.Join(got, " ") != strings.TrimSpace(want) {
			t.Errorf("line %d %q: got %v", i+1, strings.TrimSpace(line), got)
		}
	}
}

func TestPromotedFieldsAndPositionalLiteralsNameFieldsImplicitly(t *testing.T) {
	facts := analyzeSources(t, map[string]string{"q.go": queue, "cases.go": cases})
	q := facts["q.go"]
	declared := map[Loc]string{}
	for _, decl := range q.Decls {
		declared[Loc{q.Path, decl.NamePos.Line, decl.NamePos.Col}] = decl.Name
	}
	var got []string
	for _, implicit := range facts["cases.go"].Implicit {
		got = append(got, declared[implicit.Target]+":"+implicit.Mode)
	}
	sort.Strings(got)
	if want := "Inner:read k:read"; strings.Join(got, " ") != want {
		t.Errorf("got %v, want %s", got, want)
	}
}

const dispatch = `package p

type Lister interface{ List() []int }

type A struct{ s []int }

func (a *A) List() []int { return a.s }

type B struct{}

func (B) List() []int { return nil }

// C implements Lister, but no code makes it the value of an interface.
type C struct{ s []int }

func (c *C) List() []int { return c.s }

// D does, only as the type argument of a generic function.
type D struct{ s []int }

func (d *D) List() []int { return d.s }

type Wrapped struct{ Lister }

func first(l Lister) int { return l.List()[0] }

func generic[T Lister](t T) { t.List() }

func use() {
	first(&A{})
	var _ Lister = B{}
	generic(&D{})
	w := Wrapped{&A{}}
	w.List()
}
`

func TestInterfaceCallsDispatchToTheTypesConvertedToInterfaces(t *testing.T) {
	facts := analyzeSources(t, map[string]string{"d.go": dispatch})
	file := facts["d.go"]
	names := map[Loc]string{}
	for _, decl := range file.Decls {
		if decl.Parent != nil {
			for _, owner := range file.Decls {
				if (Loc{file.Path, owner.NamePos.Line, owner.NamePos.Col}) == *decl.Parent {
					names[Loc{file.Path, decl.NamePos.Line, decl.NamePos.Col}] = owner.Name + "." + decl.Name
				}
			}
		}
	}
	lines := strings.Split(dispatch, "\n")
	got := map[string][]string{}
	for _, implicit := range file.Implicit {
		if !implicit.Dispatched {
			continue
		}
		line := strings.TrimSpace(lines[implicit.Pos.Line])
		got[line] = append(got[line], names[implicit.Target]+":"+implicit.Mode)
	}
	want := map[string]string{
		"func first(l Lister) int { return l.List()[0] }": "A.List:read B.List:read D.List:read",
		"func generic[T Lister](t T) { t.List() }":        "A.List:read B.List:read D.List:read",
		"w.List()": "A.List:read B.List:read D.List:read",
	}
	for line, targets := range want {
		sort.Strings(got[line])
		if strings.Join(got[line], " ") != targets {
			t.Errorf("%s: got %v, want %s", line, got[line], targets)
		}
	}
	if len(got) != len(want) {
		t.Errorf("dispatches at %v", got)
	}
}

const aliasSource = `package p

type Queue struct{ items []int }

type Alias = Queue

func (q *Alias) Items() []int { return q.items }

func (q *Alias) Set(items []int) { q.items = items }
`

func TestMethodsDeclaredOnAnAliasBelongToTheAliasedType(t *testing.T) {
	file := analyzeSources(t, map[string]string{"a.go": aliasSource})["a.go"]
	var queue, items Loc
	for _, decl := range file.Decls {
		at := Loc{file.Path, decl.NamePos.Line, decl.NamePos.Col}
		switch decl.Name {
		case "Queue":
			queue = at
		case "items":
			items = at
		}
	}
	for _, decl := range file.Decls {
		switch decl.Name {
		case "Items":
			if decl.Parent == nil || *decl.Parent != queue || len(decl.Getter) != 1 || decl.Getter[0].Field != items || !decl.Getter[0].Lent {
				t.Errorf("Items: %+v", decl)
			}
		case "Set":
			if decl.Parent == nil || *decl.Parent != queue || len(decl.Setter) != 1 || decl.Setter[0] != items {
				t.Errorf("Set: %+v", decl)
			}
		}
	}
}
