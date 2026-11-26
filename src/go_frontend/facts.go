// This program is embedded in the frontend and run with the Go toolchain. It
// type-checks the indexed packages from source, all in one universe, and
// reports by source position what the SCIP index does not say: which type
// each declaration belongs to, what each use of a field or function does with
// it, which methods only hand out, overwrite or pass on, and the references
// the source does not spell out (promoted fields, positional struct literals,
// and the methods an interface call may reach).
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"go/ast"
	"go/importer"
	"go/parser"
	"go/token"
	"go/types"
	"io"
	"os"
	"os/exec"
	"runtime"
	"sort"
	"strings"
)

const (
	read   = "read"
	mutate = "mutate"
)

type Pos struct {
	Line int `json:"line"`
	Col  int `json:"col"`
}

// Loc is where a workspace object is declared: the position of its name.
type Loc struct {
	Path string `json:"path"`
	Line int    `json:"line"`
	Col  int    `json:"col"`
}

// A field a getter returns, and whether what it returns shares the field's
// storage.
type Result struct {
	Field Loc  `json:"field"`
	Lent  bool `json:"lent"`
}

// A call that passes every parameter on, possibly on fields of the receiver.
type Forward struct {
	Target Loc   `json:"target"`
	Fields []Loc `json:"fields"`
}

type Decl struct {
	Name     string   `json:"name"`
	Kind     string   `json:"kind"`
	Parent   *Loc     `json:"parent,omitempty"`
	Start    Pos      `json:"start"`
	End      Pos      `json:"end"`
	NamePos  Pos      `json:"name_pos"`
	Receiver bool     `json:"receiver"`
	Getter   []Result `json:"getter,omitempty"`
	Setter   []Loc    `json:"setter,omitempty"`
	Forward  *Forward `json:"forward,omitempty"`
}

// What the code at Pos does with the field it names or with what the
// function it names returns: only reads a value, or may write or keep a way
// to the storage.
type Use struct {
	Pos  Pos    `json:"pos"`
	Mode string `json:"mode"`
}

// A reference the source does not spell out at Pos: a promoted field's
// embedded field, a field set by a positional struct literal, or, when
// Dispatched, a method an interface method call may run.
type Implicit struct {
	Pos        Pos    `json:"pos"`
	Target     Loc    `json:"target"`
	Mode       string `json:"mode"`
	Dispatched bool   `json:"dispatched"`
}

type File struct {
	Path       string     `json:"path"`
	Package    string     `json:"package"`
	ImportPath string     `json:"import_path"`
	Lines      int        `json:"lines"`
	Decls      []Decl     `json:"decls"`
	Uses       []Use      `json:"uses"`
	Implicit   []Implicit `json:"implicit"`
}

type Input struct {
	// The directory `go list` resolves dependencies from.
	Dir      string         `json:"dir"`
	Packages []InputPackage `json:"packages"`
}

// The files of one directory, and its import path.
type InputPackage struct {
	Dir        string   `json:"dir"`
	ImportPath string   `json:"import_path"`
	Files      []string `json:"files"`
}

type Package struct {
	path  string
	files []*ast.File
	paths []string
	types *types.Package
	info  *types.Info
	state int
	err   error
}

type Universe struct {
	fset      *token.FileSet
	dir       string
	packages  []*Package
	byPath    map[string]*Package
	workspace map[string]bool
	exports   map[string]string
	gc        types.Importer
	sizes     types.Sizes
	// The concrete types that become dynamic types of interface values.
	dynamic     []types.Type
	dynamicKeys map[string]bool
	impls       map[*types.Func][]*types.Func
	// The declarations reported, which references may name.
	declared map[Loc]bool
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, err)
	os.Exit(1)
}

func goCommand(dir string, args ...string) *exec.Cmd {
	cmd := exec.Command("go", args...)
	cmd.Dir = dir
	cmd.Env = os.Environ()
	for _, key := range []string{"GOOS", "GOARCH", "GOFLAGS", "GO111MODULE"} {
		value, _ := os.LookupEnv("DESLOP_TARGET_" + key)
		cmd.Env = append(cmd.Env, key+"="+value)
	}
	return cmd
}

// Finds export data of dependencies with one `go list` for all of them; a
// path it did not report is looked up on its own.
func (u *Universe) prefetch(paths []string) {
	if len(paths) == 0 {
		return
	}
	args := append([]string{"list", "-e", "-export", "-f", "{{.ImportPath}}\t{{.Export}}", "--"}, paths...)
	var stderr bytes.Buffer
	cmd := goCommand(u.dir, args...)
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		fail(fmt.Errorf("go list -export: %s: %w", strings.TrimSpace(stderr.String()), err))
	}
	scanner := bufio.NewScanner(bytes.NewReader(out))
	for scanner.Scan() {
		path, export, ok := strings.Cut(scanner.Text(), "\t")
		if ok && export != "" {
			u.exports[path] = export
		}
	}
}

func (u *Universe) lookup(path string) (io.ReadCloser, error) {
	export, ok := u.exports[path]
	if !ok {
		out, err := goCommand(u.dir, "list", "-export", "-f", "{{.Export}}", "--", path).CombinedOutput()
		if err != nil {
			return nil, fmt.Errorf("go list %s: %s: %w", path, strings.TrimSpace(string(out)), err)
		}
		export = strings.TrimSpace(string(out))
	}
	if export == "" {
		return nil, fmt.Errorf("no export data for %s", path)
	}
	return os.Open(export)
}

func (u *Universe) Import(path string) (*types.Package, error) {
	if path == "unsafe" {
		return types.Unsafe, nil
	}
	if p := u.byPath[path]; p != nil {
		return u.check(p)
	}
	return u.gc.Import(path)
}

// Type-checks a workspace package from source, its workspace imports first,
// so that every package sees the same objects for the same declarations.
func (u *Universe) check(p *Package) (*types.Package, error) {
	switch p.state {
	case 1:
		return nil, fmt.Errorf("import cycle through %s", p.path)
	case 2:
		return p.types, p.err
	}
	p.state = 1
	p.info = &types.Info{
		Types:      map[ast.Expr]types.TypeAndValue{},
		Defs:       map[*ast.Ident]types.Object{},
		Uses:       map[*ast.Ident]types.Object{},
		Selections: map[*ast.SelectorExpr]*types.Selection{},
		Instances:  map[*ast.Ident]types.Instance{},
	}
	var first error
	config := types.Config{
		Importer:    u,
		Sizes:       u.sizes,
		FakeImportC: true,
		Error: func(err error) {
			if first == nil {
				first = err
			}
		},
	}
	p.types, _ = config.Check(p.path, u.fset, p.files, p.info)
	if first != nil {
		p.err = fmt.Errorf("cannot type-check Go package %s: %w", p.path, first)
	}
	p.state = 2
	return p.types, p.err
}

func (u *Universe) pos(p token.Pos) Pos {
	q := u.fset.PositionFor(p, false)
	return Pos{q.Line - 1, q.Column - 1}
}

// Where obj is declared, if the workspace declares it.
func (u *Universe) loc(obj types.Object) *Loc {
	switch o := obj.(type) {
	case nil:
		return nil
	case *types.Var:
		if o == nil {
			return nil
		}
		obj = o.Origin()
	case *types.Func:
		if o == nil {
			return nil
		}
		obj = o.Origin()
	case *types.TypeName:
		if o == nil {
			return nil
		}
	}
	if !obj.Pos().IsValid() {
		return nil
	}
	q := u.fset.PositionFor(obj.Pos(), false)
	if !u.workspace[q.Filename] {
		return nil
	}
	return &Loc{q.Filename, q.Line - 1, q.Column - 1}
}

func isInterface(t types.Type) bool {
	if t == nil {
		return false
	}
	if _, ok := types.Unalias(t).(*types.TypeParam); ok {
		return false
	}
	return types.IsInterface(t)
}

func isInterfaceMethod(fn *types.Func) bool {
	sig, ok := fn.Type().(*types.Signature)
	return ok && sig.Recv() != nil && isInterface(sig.Recv().Type())
}

func pointer(t types.Type) bool {
	_, ok := types.Unalias(t).(*types.Pointer)
	return ok
}

// Whether a value of type t shares storage with its copies: through a
// pointer, slice, map, channel, function, interface, or type parameter,
// directly or inside an array, struct or tuple. A value without them is
// plain data: a copy of it is independent of the original.
func aliases(t types.Type, seen map[types.Type]bool) bool {
	if t == nil {
		return true
	}
	t = types.Unalias(t)
	if seen[t] {
		return false
	}
	seen[t] = true
	if tuple, ok := t.(*types.Tuple); ok {
		for i := range tuple.Len() {
			if aliases(tuple.At(i).Type(), seen) {
				return true
			}
		}
		return false
	}
	if _, ok := t.(*types.TypeParam); ok {
		return true
	}
	switch t := t.Underlying().(type) {
	case *types.Basic:
		return t.Kind() == types.UnsafePointer
	case *types.Array:
		return aliases(t.Elem(), seen)
	case *types.Struct:
		for i := range t.NumFields() {
			if aliases(t.Field(i).Type(), seen) {
				return true
			}
		}
		return false
	default:
		return true
	}
}

func plain(t types.Type) bool { return !aliases(t, map[types.Type]bool{}) }

func modeOf(isRead bool) string {
	if isRead {
		return read
	}
	return mutate
}

func structOf(t types.Type) *types.Struct {
	t = types.Unalias(t)
	if p, ok := t.Underlying().(*types.Pointer); ok {
		t = p.Elem()
	}
	s, _ := types.Unalias(t).Underlying().(*types.Struct)
	return s
}

func builtinOf(info *types.Info, fun ast.Expr) *types.Builtin {
	switch f := ast.Unparen(fun).(type) {
	case *ast.Ident:
		b, _ := info.Uses[f].(*types.Builtin)
		return b
	case *ast.SelectorExpr:
		b, _ := info.Uses[f.Sel].(*types.Builtin)
		return b
	}
	return nil
}

func blank(e ast.Expr) bool {
	id, ok := e.(*ast.Ident)
	return e == nil || ok && id.Name == "_"
}

// The elements a value of type t holds when it is copied, ranged over or
// appended from.
func elements(t types.Type) (key, value types.Type) {
	u := types.Unalias(t).Underlying()
	if p, ok := u.(*types.Pointer); ok {
		u = types.Unalias(p.Elem()).Underlying()
	}
	switch u := u.(type) {
	case *types.Slice:
		return types.Typ[types.Int], u.Elem()
	case *types.Array:
		return types.Typ[types.Int], u.Elem()
	case *types.Map:
		return u.Key(), u.Elem()
	case *types.Chan:
		return u.Elem(), nil
	case *types.Basic:
		if u.Info()&types.IsString != 0 {
			return types.Typ[types.Int], types.Typ[types.Rune]
		}
		return u, nil
	}
	return nil, nil
}

// What handing on the value of e does: a copy of plain data only reads it,
// anything else keeps a way to the storage.
func valueMode(info *types.Info, e ast.Expr) string { return modeOf(plain(info.TypeOf(e))) }

// What calling or binding method fn on a value does with that value: a
// pointer or interface receiver may write through it.
func receiverMode(fn *types.Func) string {
	sig := fn.Type().(*types.Signature)
	recv := sig.Recv().Type()
	if isInterface(recv) || pointer(recv) {
		return mutate
	}
	return modeOf(plain(recv))
}

// What the code does with e, a field selection or a call, by the expressions
// and statements around it: whether it only reads a value or may write the
// storage or keep a way to it. stack holds e's ancestors, innermost last.
func useMode(info *types.Info, e ast.Expr, stack []ast.Node) string {
	for i := len(stack) - 1; i >= 0; i-- {
		switch p := stack[i].(type) {
		case *ast.ParenExpr, *ast.StarExpr:
			e = p.(ast.Expr)
		case *ast.SelectorExpr:
			s := info.Selections[p]
			if s == nil || p.X != e {
				return mutate
			}
			if s.Kind() != types.FieldVal {
				return receiverMode(s.Obj().(*types.Func))
			}
			e = p
		case *ast.IndexExpr:
			if p.X != e {
				return valueMode(info, e)
			}
			e = p
		case *ast.SliceExpr:
			if p.X != e {
				return read
			}
			e = p
		case *ast.TypeAssertExpr:
			e = p
		case *ast.UnaryExpr:
			if p.Op == token.AND || p.Op == token.ARROW {
				return mutate
			}
			return read
		case *ast.BinaryExpr, *ast.IndexListExpr, *ast.ExprStmt, *ast.IfStmt, *ast.SwitchStmt,
			*ast.CaseClause, *ast.ForStmt, *ast.GoStmt, *ast.DeferStmt:
			return read
		case *ast.CallExpr:
			if p.Fun == e {
				return mutate
			}
			if tv, ok := info.Types[p.Fun]; ok && tv.IsType() {
				from := types.Unalias(info.TypeOf(e))
				_, toString := types.Unalias(tv.Type).Underlying().(*types.Basic)
				if plain(from) || toString {
					return read
				}
				e = p
				continue
			}
			return argumentMode(info, p, e)
		case *ast.AssignStmt:
			for _, lhs := range p.Lhs {
				if lhs == e {
					return mutate
				}
			}
			return valueMode(info, e)
		case *ast.IncDecStmt:
			return mutate
		case *ast.SendStmt:
			if p.Chan == e {
				return mutate
			}
			return valueMode(info, e)
		case *ast.RangeStmt:
			if p.X != e {
				return mutate
			}
			return rangeMode(info, p)
		default:
			return valueMode(info, e)
		}
	}
	return valueMode(info, e)
}

func argumentMode(info *types.Info, call *ast.CallExpr, e ast.Expr) string {
	b := builtinOf(info, call.Fun)
	if b == nil {
		return valueMode(info, e)
	}
	first := len(call.Args) > 0 && call.Args[0] == e
	switch b.Name() {
	case "len", "cap", "print", "println", "complex", "real", "imag", "min", "max",
		"Sizeof", "Alignof", "Offsetof", "new", "make":
		return read
	case "append":
		if first {
			return mutate
		}
		if call.Ellipsis.IsValid() && call.Args[len(call.Args)-1] == e {
			_, value := elements(info.TypeOf(e))
			return modeOf(value != nil && plain(value))
		}
		return valueMode(info, e)
	case "copy":
		if first {
			return mutate
		}
		_, value := elements(info.TypeOf(e))
		return modeOf(value != nil && plain(value))
	case "delete", "clear", "close":
		if first {
			return mutate
		}
		return valueMode(info, e)
	case "panic":
		return valueMode(info, e)
	}
	// unsafe.Add, unsafe.Slice, unsafe.String and the like hand out pointers.
	return mutate
}

func rangeMode(info *types.Info, r *ast.RangeStmt) string {
	t := types.Unalias(info.TypeOf(r.X))
	if t == nil {
		return mutate
	}
	switch t.Underlying().(type) {
	case *types.Chan, *types.Signature, *types.Interface:
		return mutate
	case *types.Basic:
		return read
	}
	key, value := elements(t)
	if key == nil || value == nil {
		return mutate
	}
	if !blank(r.Key) && !plain(key) || !blank(r.Value) && !plain(value) {
		return mutate
	}
	return read
}

// What the code does with what a call of the function or method named by ref
// returns; a function taken as a value may be called anywhere with anything.
func callMode(info *types.Info, ref ast.Expr, stack []ast.Node) string {
	e, i := ref, len(stack)-1
	for ; i >= 0; i-- {
		switch p := stack[i].(type) {
		case *ast.ParenExpr:
			e = p
			continue
		case *ast.IndexExpr:
			if p.X == e {
				e = p
				continue
			}
		case *ast.IndexListExpr:
			if p.X == e {
				e = p
				continue
			}
		}
		break
	}
	if i >= 0 {
		if call, ok := stack[i].(*ast.CallExpr); ok && call.Fun == e {
			return useMode(info, call, stack[:i])
		}
	}
	return mutate
}

func (u *Universe) addDynamic(t types.Type) {
	if t == nil {
		return
	}
	t = types.Unalias(t)
	if isInterface(t) {
		return
	}
	switch t.(type) {
	case *types.TypeParam, *types.Tuple:
		return
	}
	if types.NewMethodSet(t).Len() == 0 && types.NewMethodSet(types.NewPointer(t)).Len() == 0 {
		return
	}
	key := types.TypeString(t, nil)
	if !u.dynamicKeys[key] {
		u.dynamicKeys[key] = true
		u.dynamic = append(u.dynamic, t)
	}
}

func (u *Universe) convert(from, to types.Type) {
	if to != nil && isInterface(to) {
		u.addDynamic(from)
	}
}

// The types of values, or of the results of the single call that is all of
// them.
func valueTypes(info *types.Info, values []ast.Expr) []types.Type {
	if len(values) == 1 {
		if tuple, ok := info.TypeOf(values[0]).(*types.Tuple); ok {
			return tupleTypes(tuple)
		}
	}
	out := make([]types.Type, len(values))
	for i, value := range values {
		out[i] = info.TypeOf(value)
	}
	return out
}

func (u *Universe) convertAll(info *types.Info, values []ast.Expr, targets []types.Type) {
	for i, from := range valueTypes(info, values) {
		if i < len(targets) {
			u.convert(from, targets[i])
		}
	}
}

func tupleTypes(t *types.Tuple) []types.Type {
	out := make([]types.Type, t.Len())
	for i := range out {
		out[i] = t.At(i).Type()
	}
	return out
}

// Collects the dynamic types of interface values: every concrete type the
// workspace's code converts to an interface, explicitly or by assignment,
// argument, result, literal element, send or map key, and every type
// argument, whose values generic code may convert.
func (u *Universe) collectDynamic(p *Package, f *ast.File) {
	info := p.info
	var stack []ast.Node
	signature := func() *types.Signature {
		for i := len(stack) - 1; i >= 0; i-- {
			switch fn := stack[i].(type) {
			case *ast.FuncLit:
				sig, _ := info.TypeOf(fn).(*types.Signature)
				return sig
			case *ast.FuncDecl:
				if obj, ok := info.Defs[fn.Name].(*types.Func); ok {
					return obj.Type().(*types.Signature)
				}
				return nil
			}
		}
		return nil
	}
	ast.Inspect(f, func(n ast.Node) bool {
		if n == nil {
			stack = stack[:len(stack)-1]
			return false
		}
		switch n := n.(type) {
		case *ast.AssignStmt:
			if n.Tok == token.ASSIGN || n.Tok == token.DEFINE {
				targets := make([]types.Type, len(n.Lhs))
				for i, lhs := range n.Lhs {
					targets[i] = info.TypeOf(lhs)
				}
				u.convertAll(info, n.Rhs, targets)
			}
		case *ast.ValueSpec:
			if n.Type != nil {
				targets := make([]types.Type, len(n.Names))
				for i := range targets {
					targets[i] = info.TypeOf(n.Type)
				}
				u.convertAll(info, n.Values, targets)
			}
		case *ast.ReturnStmt:
			if sig := signature(); sig != nil {
				u.convertAll(info, n.Results, tupleTypes(sig.Results()))
			}
		case *ast.CallExpr:
			if tv, ok := info.Types[n.Fun]; ok && tv.IsType() {
				if len(n.Args) == 1 {
					u.convert(info.TypeOf(n.Args[0]), tv.Type)
				}
				break
			}
			if b := builtinOf(info, n.Fun); b != nil {
				switch b.Name() {
				case "append":
					if _, value := elements(info.TypeOf(n)); value != nil && !n.Ellipsis.IsValid() {
						for _, arg := range n.Args[1:] {
							u.convert(info.TypeOf(arg), value)
						}
					}
				case "panic":
					if len(n.Args) == 1 {
						u.convert(info.TypeOf(n.Args[0]), types.Universe.Lookup("any").Type())
					}
				}
				break
			}
			sig, ok := types.Unalias(info.TypeOf(n.Fun)).Underlying().(*types.Signature)
			if !ok {
				break
			}
			params := tupleTypes(sig.Params())
			for i, from := range valueTypes(info, n.Args) {
				var to types.Type
				switch {
				case sig.Variadic() && i >= len(params)-1:
					to = params[len(params)-1]
					if slice, ok := to.Underlying().(*types.Slice); ok && !n.Ellipsis.IsValid() {
						to = slice.Elem()
					}
				case i < len(params):
					to = params[i]
				}
				u.convert(from, to)
			}
		case *ast.CompositeLit:
			t := info.TypeOf(n)
			if t == nil {
				break
			}
			if p, ok := types.Unalias(t).Underlying().(*types.Pointer); ok {
				t = p.Elem()
			}
			switch lit := types.Unalias(t).Underlying().(type) {
			case *types.Struct:
				for i, elt := range n.Elts {
					if kv, ok := elt.(*ast.KeyValueExpr); ok {
						if id, ok := kv.Key.(*ast.Ident); ok {
							if field, ok := info.Uses[id].(*types.Var); ok {
								u.convert(info.TypeOf(kv.Value), field.Type())
							}
						}
					} else if i < lit.NumFields() {
						u.convert(info.TypeOf(elt), lit.Field(i).Type())
					}
				}
			case *types.Map:
				for _, elt := range n.Elts {
					if kv, ok := elt.(*ast.KeyValueExpr); ok {
						u.convert(info.TypeOf(kv.Key), lit.Key())
						u.convert(info.TypeOf(kv.Value), lit.Elem())
					}
				}
			case *types.Slice, *types.Array:
				_, value := elements(t)
				for _, elt := range n.Elts {
					if kv, ok := elt.(*ast.KeyValueExpr); ok {
						elt = kv.Value
					}
					u.convert(info.TypeOf(elt), value)
				}
			}
		case *ast.SendStmt:
			if ch, ok := types.Unalias(info.TypeOf(n.Chan)).Underlying().(*types.Chan); ok {
				u.convert(info.TypeOf(n.Value), ch.Elem())
			}
		case *ast.IndexExpr:
			if m, ok := types.Unalias(info.TypeOf(n.X)).Underlying().(*types.Map); ok {
				u.convert(info.TypeOf(n.Index), m.Key())
			}
		case *ast.RangeStmt:
			if n.Tok == token.ASSIGN {
				key, value := elements(info.TypeOf(n.X))
				if !blank(n.Key) {
					u.convert(key, info.TypeOf(n.Key))
				}
				if !blank(n.Value) {
					u.convert(value, info.TypeOf(n.Value))
				}
			}
		}
		stack = append(stack, n)
		return true
	})
	file := u.fset.File(f.Pos())
	for ident, instance := range info.Instances {
		if u.fset.File(ident.Pos()) != file {
			continue
		}
		for i := range instance.TypeArgs.Len() {
			arg := instance.TypeArgs.At(i)
			u.addDynamic(arg)
			if !pointer(arg) {
				u.addDynamic(types.NewPointer(arg))
			}
		}
	}
}

// The workspace methods a call of the interface method fn may run: the
// methods of the dynamic types that implement its interface, and through an
// embedded interface, those its dynamic types run.
func (u *Universe) implementations(fn *types.Func, seen map[*types.Func]bool) []*types.Func {
	if found, ok := u.impls[fn]; ok {
		return found
	}
	seen[fn] = true
	iface, _ := types.Unalias(fn.Type().(*types.Signature).Recv().Type()).Underlying().(*types.Interface)
	var found []*types.Func
	if iface != nil {
		for _, t := range u.dynamic {
			if !types.Implements(t, iface) {
				continue
			}
			obj, _, _ := types.LookupFieldOrMethod(t, false, fn.Pkg(), fn.Name())
			method, ok := obj.(*types.Func)
			if !ok {
				continue
			}
			if isInterfaceMethod(method) {
				if !seen[method] {
					found = append(found, u.implementations(method, seen)...)
				}
				continue
			}
			found = append(found, method.Origin())
		}
	}
	sort.Slice(found, func(i, j int) bool { return found[i].Pos() < found[j].Pos() })
	unique := found[:0]
	for i, method := range found {
		if i == 0 || method != found[i-1] {
			unique = append(unique, method)
		}
	}
	u.impls[fn] = unique
	return unique
}

func embeddedIdent(e ast.Expr) *ast.Ident {
	for {
		switch t := e.(type) {
		case *ast.StarExpr:
			e = t.X
		case *ast.ParenExpr:
			e = t.X
		case *ast.IndexExpr:
			e = t.X
		case *ast.IndexListExpr:
			e = t.X
		case *ast.SelectorExpr:
			return t.Sel
		case *ast.Ident:
			return t
		default:
			return nil
		}
	}
}

// The named type a method is declared on, through aliases and instances.
func receiverBase(fn *types.Func) types.Object {
	t := types.Unalias(fn.Type().(*types.Signature).Recv().Type())
	if p, ok := t.(*types.Pointer); ok {
		t = types.Unalias(p.Elem())
	}
	if named, ok := t.(*types.Named); ok {
		return named.Origin().Obj()
	}
	return nil
}

// Marks decl as a getter, setter or forwarder if d's body is only that.
func (u *Universe) wrapper(info *types.Info, decl *Decl, d *ast.FuncDecl) {
	if d.Body == nil || len(d.Body.List) == 0 {
		return
	}
	var recv types.Object
	byPointer := false
	if d.Recv != nil && len(d.Recv.List) == 1 && len(d.Recv.List[0].Names) == 1 {
		recv = info.Defs[d.Recv.List[0].Names[0]]
		byPointer = recv != nil && pointer(recv.Type())
	}
	// recv.field, for a field declared in the receiver's own type.
	ownField := func(e ast.Expr) (*types.Var, *ast.SelectorExpr) {
		sel, ok := ast.Unparen(e).(*ast.SelectorExpr)
		if !ok || recv == nil {
			return nil, nil
		}
		x, ok := ast.Unparen(sel.X).(*ast.Ident)
		s := info.Selections[sel]
		if !ok || info.Uses[x] != recv || s == nil || s.Kind() != types.FieldVal || len(s.Index()) != 1 {
			return nil, nil
		}
		return s.Obj().(*types.Var), sel
	}
	var params []types.Object
	named := true
	for _, field := range d.Type.Params.List {
		if len(field.Names) == 0 {
			named = false
		}
		for _, name := range field.Names {
			params = append(params, info.Defs[name])
		}
	}
	isParam := func(obj types.Object) bool {
		for _, p := range params {
			if obj != nil && p == obj {
				return true
			}
		}
		return false
	}
	single := len(d.Body.List) == 1

	if ret, ok := d.Body.List[0].(*ast.ReturnStmt); single && ok && len(params) == 0 && named && len(ret.Results) > 0 {
		var results []Result
		for _, r := range ret.Results {
			r = ast.Unparen(r)
			address := false
			if un, ok := r.(*ast.UnaryExpr); ok && un.Op == token.AND {
				address, r = true, un.X
			}
			field, sel := ownField(r)
			l := u.loc(field)
			if l == nil {
				results = nil
				break
			}
			results = append(results, Result{*l, aliases(info.TypeOf(sel), map[types.Type]bool{}) || address && byPointer})
		}
		if results != nil {
			decl.Getter = results
			return
		}
	}

	if byPointer && named && len(params) > 0 {
		used := map[types.Object]bool{}
		var fields []Loc
		ok := true
		for _, stmt := range d.Body.List {
			assign, isAssign := stmt.(*ast.AssignStmt)
			if !isAssign || assign.Tok != token.ASSIGN || len(assign.Lhs) != len(assign.Rhs) {
				ok = false
				break
			}
			for i, lhs := range assign.Lhs {
				field, _ := ownField(lhs)
				l := u.loc(field)
				id, isIdent := ast.Unparen(assign.Rhs[i]).(*ast.Ident)
				if l == nil || !isIdent || !isParam(info.Uses[id]) || used[info.Uses[id]] {
					ok = false
					break
				}
				used[info.Uses[id]] = true
				fields = append(fields, *l)
			}
		}
		if ok && len(used) == len(params) {
			decl.Setter = fields
			return
		}
	}

	var call *ast.CallExpr
	switch s := d.Body.List[0].(type) {
	case *ast.ExprStmt:
		call, _ = ast.Unparen(s.X).(*ast.CallExpr)
	case *ast.ReturnStmt:
		if len(s.Results) == 1 {
			call, _ = ast.Unparen(s.Results[0]).(*ast.CallExpr)
		}
	}
	if !single || !named || call == nil {
		return
	}
	used := map[types.Object]bool{}
	consume := func(e ast.Expr) bool {
		id, ok := ast.Unparen(e).(*ast.Ident)
		if !ok {
			return false
		}
		obj := info.Uses[id]
		if obj == nil || !(isParam(obj) || obj == recv) || used[obj] {
			return false
		}
		used[obj] = true
		return true
	}
	var target *types.Func
	var fields []Loc
	switch fun := ast.Unparen(call.Fun).(type) {
	case *ast.Ident:
		target, _ = info.Uses[fun].(*types.Func)
	case *ast.SelectorExpr:
		s := info.Selections[fun]
		switch {
		case s == nil:
			target, _ = info.Uses[fun.Sel].(*types.Func)
		case s.Kind() == types.MethodVal:
			target, _ = s.Obj().(*types.Func)
			if field, _ := ownField(fun.X); field != nil {
				if l := u.loc(field); l != nil {
					fields = append(fields, *l)
					used[recv] = true
				} else {
					return
				}
			} else if !consume(fun.X) {
				return
			}
		}
	}
	if target == nil || isInterfaceMethod(target) {
		return
	}
	if call.Ellipsis.IsValid() {
		variadic := d.Type.Params.List
		if len(variadic) == 0 {
			return
		}
		if _, ok := variadic[len(variadic)-1].Type.(*ast.Ellipsis); !ok {
			return
		}
	}
	for _, arg := range call.Args {
		if !consume(arg) {
			return
		}
	}
	for _, p := range params {
		if !used[p] {
			return
		}
	}
	if recv != nil && !used[recv] {
		return
	}
	if l := u.loc(target); l != nil {
		decl.Forward = &Forward{Target: *l, Fields: fields}
	}
}

// The declarations of f, which the later uses refer to.
func (u *Universe) declarations(p *Package, f *ast.File, path, importPath string) File {
	info := p.info
	out := File{
		Path:       path,
		Package:    f.Name.Name,
		ImportPath: importPath,
		Lines:      u.fset.File(f.Pos()).LineCount(),
		Decls:      []Decl{},
		Uses:       []Use{},
		Implicit:   []Implicit{},
	}
	add := func(name *ast.Ident, kind string, parent *Loc, node ast.Node, receiver bool) *Decl {
		at := u.pos(name.Pos())
		u.declared[Loc{path, at.Line, at.Col}] = true
		out.Decls = append(out.Decls, Decl{
			Name: name.Name, Kind: kind, Parent: parent,
			Start: u.pos(node.Pos()), End: u.pos(node.End()), NamePos: at,
			Receiver: receiver,
		})
		return &out.Decls[len(out.Decls)-1]
	}
	for _, d := range f.Decls {
		switch d := d.(type) {
		case *ast.FuncDecl:
			fn, _ := info.Defs[d.Name].(*types.Func)
			if d.Name.Name == "_" || fn == nil {
				continue
			}
			var parent *Loc
			if d.Recv != nil {
				if parent = u.loc(receiverBase(fn)); parent == nil {
					fail(fmt.Errorf("%s: cannot find the type of method %s", u.fset.Position(d.Pos()), d.Name.Name))
				}
			}
			u.wrapper(info, add(d.Name, "function", parent, d, d.Recv != nil), d)
		case *ast.GenDecl:
			for _, spec := range d.Specs {
				switch s := spec.(type) {
				case *ast.TypeSpec:
					if s.Name.Name == "_" {
						continue
					}
					add(s.Name, "type", nil, s, false)
					owner := u.loc(info.Defs[s.Name])
					// Fields of nested struct types are storage of the
					// declared type too; function types only describe calls.
					ast.Inspect(s.Type, func(n ast.Node) bool {
						switch n := n.(type) {
						case *ast.FuncType:
							return false
						case *ast.InterfaceType:
							if n == s.Type {
								for _, method := range n.Methods.List {
									for _, name := range method.Names {
										if name.Name != "_" {
											add(name, "function", owner, method, false)
										}
									}
								}
							}
							return false
						case *ast.StructType:
							for _, field := range n.Fields.List {
								names := field.Names
								if len(names) == 0 {
									if id := embeddedIdent(field.Type); id != nil {
										names = []*ast.Ident{id}
									}
								}
								for _, name := range names {
									if v, ok := info.Defs[name].(*types.Var); ok && v.IsField() && name.Name != "_" {
										add(name, "field", owner, field, false)
									}
								}
							}
						}
						return true
					})
				case *ast.ValueSpec:
					for _, name := range s.Names {
						if name.Name == "_" {
							continue
						}
						// A constant of a type its own package declares is
						// one of that type's values, as an enum's variant.
						if c, ok := info.Defs[name].(*types.Const); ok {
							if named, ok := types.Unalias(c.Type()).(*types.Named); ok && named.Obj().Pkg() == p.types {
								if owner := u.loc(named.Origin().Obj()); owner != nil {
									add(name, "variant", owner, s, false)
									continue
								}
							}
						}
						add(name, "constant", nil, s, false)
					}
				}
			}
		}
	}
	return out
}

// What the code of f does with the fields and functions it names, and what
// it reaches without naming.
func (u *Universe) uses(p *Package, f *ast.File, out *File) {
	info := p.info
	var stack []ast.Node
	ast.Inspect(f, func(n ast.Node) bool {
		if n == nil {
			stack = stack[:len(stack)-1]
			return false
		}
		switch n := n.(type) {
		case *ast.SelectorExpr:
			u.selector(info, out, n, stack)
		case *ast.Ident:
			var parent ast.Node
			if len(stack) > 0 {
				parent = stack[len(stack)-1]
			}
			if sel, ok := parent.(*ast.SelectorExpr); ok && sel.Sel == n {
				break
			}
			switch obj := info.Uses[n].(type) {
			case *types.Func:
				out.Uses = append(out.Uses, Use{u.pos(n.Pos()), callMode(info, n, stack)})
			case *types.Var:
				// A struct literal gives each field its value whole, as a
				// constructor does.
				if kv, ok := parent.(*ast.KeyValueExpr); ok && kv.Key == n && obj.IsField() {
					out.Uses = append(out.Uses, Use{u.pos(n.Pos()), read})
				}
			}
		case *ast.CompositeLit:
			u.positional(info, out, n)
		}
		stack = append(stack, n)
		return true
	})
}

func (u *Universe) selector(info *types.Info, out *File, n *ast.SelectorExpr, stack []ast.Node) {
	at := u.pos(n.Sel.Pos())
	s := info.Selections[n]
	if s == nil {
		if _, ok := info.Uses[n.Sel].(*types.Func); ok {
			out.Uses = append(out.Uses, Use{at, callMode(info, n, stack)})
		}
		return
	}
	if s.Kind() == types.FieldVal {
		mode := useMode(info, n, stack)
		out.Uses = append(out.Uses, Use{at, mode})
		// A promoted field is reached through the embedded fields on the way.
		t := s.Recv()
		for _, index := range s.Index()[:len(s.Index())-1] {
			st := structOf(t)
			if st == nil {
				break
			}
			field := st.Field(index)
			if l := u.item(field); l != nil {
				out.Implicit = append(out.Implicit, Implicit{Pos: at, Target: *l, Mode: mode})
			}
			t = field.Type()
		}
		return
	}
	mode := callMode(info, n, stack)
	out.Uses = append(out.Uses, Use{at, mode})
	fn := s.Obj().(*types.Func)
	if isInterfaceMethod(fn) {
		for _, method := range u.implementations(fn, map[*types.Func]bool{}) {
			if l := u.item(method); l != nil {
				out.Implicit = append(out.Implicit, Implicit{Pos: at, Target: *l, Mode: mode, Dispatched: true})
			}
		}
	}
}

// Where obj is declared, if it is a declaration reported: no field of a type
// declared in a function is.
func (u *Universe) item(obj types.Object) *Loc {
	l := u.loc(obj)
	if l == nil || !u.declared[*l] {
		return nil
	}
	return l
}

// A struct literal without keys gives every field its value whole, by its
// place.
func (u *Universe) positional(info *types.Info, out *File, lit *ast.CompositeLit) {
	if len(lit.Elts) == 0 {
		return
	}
	if _, keyed := lit.Elts[0].(*ast.KeyValueExpr); keyed {
		return
	}
	st := structOf(info.TypeOf(lit))
	if st == nil {
		return
	}
	for i, elt := range lit.Elts {
		if i >= st.NumFields() {
			break
		}
		if l := u.item(st.Field(i)); l != nil {
			out.Implicit = append(out.Implicit, Implicit{Pos: u.pos(elt.Pos()), Target: *l, Mode: read})
		}
	}
}

func main() {
	var input Input
	if err := json.NewDecoder(os.Stdin).Decode(&input); err != nil {
		fail(err)
	}
	if err := json.NewEncoder(os.Stdout).Encode(analyze(input)); err != nil {
		fail(err)
	}
}

func analyze(input Input) []File {
	arch := os.Getenv("DESLOP_TARGET_GOARCH")
	if arch == "" {
		arch = runtime.GOARCH
	}
	u := &Universe{
		fset:        token.NewFileSet(),
		dir:         input.Dir,
		byPath:      map[string]*Package{},
		workspace:   map[string]bool{},
		exports:     map[string]string{},
		sizes:       types.SizesFor("gc", arch),
		dynamicKeys: map[string]bool{},
		impls:       map[*types.Func][]*types.Func{},
		declared:    map[Loc]bool{},
	}
	if u.sizes == nil {
		fail(fmt.Errorf("unsupported Go target architecture %q", arch))
	}
	u.gc = importer.ForCompiler(u.fset, "gc", u.lookup)
	type located struct {
		file       *ast.File
		path       string
		pkg        *Package
		importPath string
	}
	var files []located
	imports := map[string]bool{}
	for _, in := range input.Packages {
		byName := map[string]*Package{}
		var order []string
		for _, path := range in.Files {
			u.workspace[path] = true
			f, err := parser.ParseFile(u.fset, path, nil, parser.ParseComments|parser.AllErrors)
			if err != nil {
				fail(err)
			}
			p := byName[f.Name.Name]
			if p == nil {
				p = &Package{}
				byName[f.Name.Name] = p
				order = append(order, f.Name.Name)
			}
			p.files = append(p.files, f)
			p.paths = append(p.paths, path)
			for _, spec := range f.Imports {
				imports[strings.Trim(spec.Path.Value, "`\"")] = true
			}
		}
		sort.Strings(order)
		if len(order) > 2 || len(order) == 2 && order[1] != order[0]+"_test" {
			fail(fmt.Errorf("%s: files of packages %s", in.Dir, strings.Join(order, ", ")))
		}
		for i, name := range order {
			p := byName[name]
			p.path = in.ImportPath
			if i == 1 {
				// The external test package, which no package imports.
				p.path += "_test"
			}
			if u.byPath[p.path] != nil {
				fail(fmt.Errorf("Go package %s is in more than one directory", p.path))
			}
			u.byPath[p.path] = p
			u.packages = append(u.packages, p)
			for j, f := range p.files {
				files = append(files, located{f, p.paths[j], p, p.path})
			}
		}
	}
	var external []string
	for path := range imports {
		if u.byPath[path] == nil && path != "C" && path != "unsafe" {
			external = append(external, path)
		}
	}
	sort.Strings(external)
	u.prefetch(external)
	for _, p := range u.packages {
		if _, err := u.check(p); err != nil {
			fail(err)
		}
	}
	// Tests are programs of their own: what they convert to interfaces is
	// no dynamic type of the code they test.
	for _, l := range files {
		if !strings.HasSuffix(l.path, "_test.go") {
			u.collectDynamic(l.pkg, l.file)
		}
	}
	sort.SliceStable(u.dynamic, func(i, j int) bool {
		return types.TypeString(u.dynamic[i], nil) < types.TypeString(u.dynamic[j], nil)
	})
	result := make([]File, len(files))
	for i, l := range files {
		result[i] = u.declarations(l.pkg, l.file, l.path, l.importPath)
	}
	for i, l := range files {
		u.uses(l.pkg, l.file, &result[i])
	}
	sort.Slice(result, func(i, j int) bool { return result[i].Path < result[j].Path })
	return result
}
