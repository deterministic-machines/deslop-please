// This program is embedded in the frontend and run with the Go toolchain.
package main

import (
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
	"path/filepath"
	"runtime"
	"strings"
)

type Pos struct {
	Line int `json:"line"`
	Col  int `json:"col"`
}
type Decl struct {
	Name       string `json:"name"`
	Kind       string `json:"kind"`
	Parent     string `json:"parent,omitempty"`
	Start      Pos    `json:"start"`
	End        Pos    `json:"end"`
	NamePos    Pos    `json:"name_pos"`
	Receiver   bool   `json:"receiver"`
	Forward    *Pos   `json:"forward,omitempty"`
	Setter     *Pos   `json:"setter,omitempty"`
	Getter     *Pos   `json:"getter,omitempty"`
	GetterLent bool   `json:"getter_lent"`
}
type Use struct {
	Pos  Pos    `json:"pos"`
	Mode string `json:"mode"`
}
type File struct {
	Path    string `json:"path"`
	Package string `json:"package"`
	Lines   int    `json:"lines"`
	Decls   []Decl `json:"decls"`
	Uses    []Use  `json:"uses"`
}

// A copied Go value can still expose mutable storage through its components.
func aliases(t types.Type, seen map[types.Type]bool) bool {
	if t == nil {
		return true
	}
	t = types.Unalias(t)
	if seen[t] {
		return false
	}
	seen[t] = true
	switch t := t.Underlying().(type) {
	case *types.Basic:
		return t.Kind() == types.UnsafePointer
	case *types.Array:
		return aliases(t.Elem(), seen)
	case *types.Struct:
		for i := 0; i < t.NumFields(); i++ {
			if aliases(t.Field(i).Type(), seen) {
				return true
			}
		}
		return false
	default:
		return true
	}
}

func mainError(err error) { fmt.Fprintln(os.Stderr, err); os.Exit(1) }

// Resolve imports in the package's own module/workspace, using the same build
// configuration as the indexer. Export data includes transitive dependencies.
func typeInfo(fset *token.FileSet, dir, name string, files []*ast.File) (*types.Info, error) {
	lookup := func(path string) (io.ReadCloser, error) {
		cmd := exec.Command("go", "list", "-export", "-f", "{{.Export}}", path)
		cmd.Dir = dir
		cmd.Env = os.Environ()
		for _, key := range []string{"GOOS", "GOARCH", "GOFLAGS", "GO111MODULE"} {
			value, _ := os.LookupEnv("DESLOP_TARGET_" + key)
			cmd.Env = append(cmd.Env, key+"="+value)
		}
		out, err := cmd.CombinedOutput()
		if err != nil {
			return nil, fmt.Errorf("go list %s: %s: %w", path, strings.TrimSpace(string(out)), err)
		}
		return os.Open(strings.TrimSpace(string(out)))
	}
	info := &types.Info{Types: map[ast.Expr]types.TypeAndValue{}, Defs: map[*ast.Ident]types.Object{}, Uses: map[*ast.Ident]types.Object{}, Selections: map[*ast.SelectorExpr]*types.Selection{}}
	arch := os.Getenv("DESLOP_TARGET_GOARCH")
	if arch == "" {
		arch = runtime.GOARCH
	}
	sizes := types.SizesFor("gc", arch)
	if sizes == nil {
		return nil, fmt.Errorf("unsupported Go target architecture %q", arch)
	}
	config := types.Config{Importer: importer.ForCompiler(fset, "gc", lookup), Sizes: sizes}
	_, err := config.Check(name, fset, files, info)
	return info, err
}

func fieldMode(expr ast.Expr, stack []ast.Node, info *types.Info, getter bool) string {
	if getter {
		return "read"
	}
	for i := len(stack) - 1; i >= 0; i-- {
		switch p := stack[i].(type) {
		case *ast.ParenExpr:
			if p.X != expr {
				return "unknown"
			}
			expr = p
		case *ast.SelectorExpr:
			if p.X != expr {
				return "unknown"
			}
			expr = p
		case *ast.IndexExpr:
			if p.X != expr {
				return "read"
			}
			expr = p
		case *ast.StarExpr:
			if p.X != expr {
				return "unknown"
			}
			expr = p
		case *ast.UnaryExpr:
			if p.Op == token.AND {
				return "unknown"
			}
			return "read"
		case *ast.AssignStmt:
			for _, lhs := range p.Lhs {
				if lhs == expr {
					return "mutate"
				}
			}
			if aliases(info.TypeOf(expr), map[types.Type]bool{}) {
				return "unknown"
			}
			return "read"
		case *ast.IncDecStmt:
			return "mutate"
		case *ast.BinaryExpr:
			return "read"
		case *ast.CallExpr:
			if p.Fun == expr {
				return "mutate"
			}
			if id, ok := p.Fun.(*ast.Ident); ok {
				if builtin, ok := info.Uses[id].(*types.Builtin); ok {
					if builtin.Name() == "len" || builtin.Name() == "cap" {
						return "read"
					}
					if builtin.Name() == "append" && len(p.Args) > 0 && p.Args[0] == expr {
						return "mutate"
					}
				}
			}
			if aliases(info.TypeOf(expr), map[types.Type]bool{}) {
				return "unknown"
			}
			return "read"
		case *ast.ReturnStmt, *ast.ValueSpec:
			if aliases(info.TypeOf(expr), map[types.Type]bool{}) {
				return "unknown"
			}
			return "read"
		case *ast.ExprStmt:
			return "read"
		default:
			if aliases(info.TypeOf(expr), map[types.Type]bool{}) {
				return "unknown"
			}
			return "read"
		}
	}
	return "unknown"
}
func main() {
	var paths []string
	if err := json.NewDecoder(os.Stdin).Decode(&paths); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	fset := token.NewFileSet()
	parsed := make(map[string]*ast.File, len(paths))
	groups := map[string][]*ast.File{}
	for _, path := range paths {
		f, err := parser.ParseFile(fset, path, nil, parser.AllErrors)
		if err != nil {
			mainError(err)
		}
		parsed[path] = f
		key := filepath.Dir(path) + "\x00" + f.Name.Name
		groups[key] = append(groups[key], f)
	}
	infos := map[string]*types.Info{}
	for key, files := range groups {
		parts := strings.SplitN(key, "\x00", 2)
		info, err := typeInfo(fset, parts[0], parts[1], files)
		if err != nil {
			mainError(fmt.Errorf("cannot type-check Go package %s: %w", parts[0], err))
		}
		infos[key] = info
	}
	result := make([]File, 0, len(paths))
	for _, path := range paths {
		f := parsed[path]
		info := infos[filepath.Dir(path)+"\x00"+f.Name.Name]
		pos := func(p token.Pos) Pos { q := fset.PositionFor(p, false); return Pos{q.Line - 1, q.Column - 1} }
		getters := map[*ast.SelectorExpr]bool{}
		out := File{Path: path, Package: f.Name.Name, Lines: fset.File(f.Pos()).LineCount(), Decls: []Decl{}, Uses: []Use{}}
		add := func(name *ast.Ident, kind, parent string, node ast.Node, recv bool) {
			if name == nil || name.Name == "_" {
				return
			}
			out.Decls = append(out.Decls, Decl{Name: name.Name, Kind: kind, Parent: parent, Start: pos(node.Pos()), End: pos(node.End()), NamePos: pos(name.Pos()), Receiver: recv})
		}
		for _, d := range f.Decls {
			switch d := d.(type) {
			case *ast.FuncDecl:
				parent := ""
				if d.Recv != nil && len(d.Recv.List) > 0 {
					t := d.Recv.List[0].Type
					if p, ok := t.(*ast.StarExpr); ok {
						t = p.X
					}
					switch t := t.(type) {
					case *ast.Ident:
						parent = t.Name
					case *ast.IndexExpr:
						if id, ok := t.X.(*ast.Ident); ok {
							parent = id.Name
						}
					case *ast.IndexListExpr:
						if id, ok := t.X.(*ast.Ident); ok {
							parent = id.Name
						}
					}
				}
				add(d.Name, "function", parent, d, d.Recv != nil)
				if d.Name.Name != "_" && d.Recv != nil && len(d.Recv.List) == 1 && len(d.Recv.List[0].Names) == 1 && d.Body != nil && len(d.Body.List) == 1 && d.Type.Params != nil && len(d.Type.Params.List) == 0 {
					if ret, ok := d.Body.List[0].(*ast.ReturnStmt); ok && len(ret.Results) == 1 {
						if sel, ok := ret.Results[0].(*ast.SelectorExpr); ok {
							if recv, ok := sel.X.(*ast.Ident); ok && info.Uses[recv] == info.Defs[d.Recv.List[0].Names[0]] {
								if selection := info.Selections[sel]; selection != nil && selection.Kind() == types.FieldVal {
									p := pos(sel.Sel.Pos())
									out.Decls[len(out.Decls)-1].Getter = &p
									out.Decls[len(out.Decls)-1].GetterLent = aliases(info.TypeOf(sel), map[types.Type]bool{})
									getters[sel] = true
								}
							}
						}
					}
				}
				if d.Name.Name != "_" && d.Recv != nil && len(d.Recv.List) == 1 && len(d.Recv.List[0].Names) == 1 && d.Body != nil && len(d.Body.List) == 1 && d.Type.Params != nil && len(d.Type.Params.List) == 1 && len(d.Type.Params.List[0].Names) == 1 {
					if _, pointer := d.Recv.List[0].Type.(*ast.StarExpr); pointer {
						if assign, yes := d.Body.List[0].(*ast.AssignStmt); yes && assign.Tok == token.ASSIGN && len(assign.Lhs) == 1 && len(assign.Rhs) == 1 {
							if sel, yes := assign.Lhs[0].(*ast.SelectorExpr); yes {
								receiver, receiverOK := sel.X.(*ast.Ident)
								value, valueOK := assign.Rhs[0].(*ast.Ident)
								if receiverOK && valueOK && receiver.Name == d.Recv.List[0].Names[0].Name && value.Name == d.Type.Params.List[0].Names[0].Name {
									p := pos(sel.Sel.Pos())
									out.Decls[len(out.Decls)-1].Setter = &p
								}
							}
						}
					}
				}
				if d.Name.Name != "_" && d.Body != nil && len(d.Body.List) == 1 && d.Type.Params != nil {
					var call *ast.CallExpr
					switch s := d.Body.List[0].(type) {
					case *ast.ExprStmt:
						call, _ = s.X.(*ast.CallExpr)
					case *ast.ReturnStmt:
						if len(s.Results) == 1 {
							call, _ = s.Results[0].(*ast.CallExpr)
						}
					}
					if call != nil && call.Ellipsis == token.NoPos {
						params := map[string]bool{}
						for _, field := range d.Type.Params.List {
							for _, n := range field.Names {
								params[n.Name] = false
							}
						}
						if d.Recv != nil {
							for _, field := range d.Recv.List {
								for _, n := range field.Names {
									params[n.Name] = false
								}
							}
						}
						ok := true
						consume := func(e ast.Expr) {
							if id, yes := e.(*ast.Ident); yes {
								if used, found := params[id.Name]; found && !used {
									params[id.Name] = true
									return
								}
							}
							ok = false
						}
						var target *ast.Ident
						switch fun := call.Fun.(type) {
						case *ast.Ident:
							target = fun
						case *ast.SelectorExpr:
							target = fun.Sel
							consume(fun.X)
						}
						for _, arg := range call.Args {
							consume(arg)
						}
						for _, used := range params {
							if !used {
								ok = false
							}
						}
						if ok && target != nil && len(out.Decls) > 0 {
							p := pos(target.Pos())
							out.Decls[len(out.Decls)-1].Forward = &p
						}
					}
				}
			case *ast.GenDecl:
				for _, s := range d.Specs {
					switch s := s.(type) {
					case *ast.TypeSpec:
						if s.Assign.IsValid() {
							mainError(fmt.Errorf("%s: Go type aliases cannot be represented", fset.Position(s.Pos())))
						}
						add(s.Name, "type", "", s, false)
						if st, ok := s.Type.(*ast.StructType); ok {
							for _, field := range st.Fields.List {
								for _, name := range field.Names {
									add(name, "field", s.Name.Name, field, false)
								}
								if len(field.Names) == 0 {
									t := field.Type
									if p, ok := t.(*ast.StarExpr); ok {
										t = p.X
									}
									if p, ok := t.(*ast.IndexExpr); ok {
										t = p.X
									}
									if p, ok := t.(*ast.IndexListExpr); ok {
										t = p.X
									}
									switch t := t.(type) {
									case *ast.Ident:
										add(t, "field", s.Name.Name, field, false)
									case *ast.SelectorExpr:
										add(t.Sel, "field", s.Name.Name, field, false)
									}
								}
							}
						}
						if it, ok := s.Type.(*ast.InterfaceType); ok {
							for _, field := range it.Methods.List {
								for _, name := range field.Names {
									add(name, "function", s.Name.Name, field, false)
								}
							}
						}
					case *ast.ValueSpec:
						for _, name := range s.Names {
							add(name, "constant", "", s, false)
						}
					}
				}
			}
		}
		var stack []ast.Node
		ast.Inspect(f, func(n ast.Node) bool {
			if n == nil {
				stack = stack[:len(stack)-1]
				return false
			}
			if sel, ok := n.(*ast.SelectorExpr); ok {
				if selection := info.Selections[sel]; selection != nil {
					if selection.Kind() != types.FieldVal {
						if _, dynamic := selection.Recv().Underlying().(*types.Interface); dynamic {
							mainError(fmt.Errorf("%s: interface method dispatch cannot be represented", fset.Position(sel.Pos())))
						}
						mode := "unknown"
						if len(stack) > 0 {
							if call, ok := stack[len(stack)-1].(*ast.CallExpr); ok && call.Fun == sel {
								mode = fieldMode(call, stack[:len(stack)-1], info, false)
							}
						}
						out.Uses = append(out.Uses, Use{Pos: pos(sel.Sel.Pos()), Mode: mode})
					} else {
						out.Uses = append(out.Uses, Use{Pos: pos(sel.Sel.Pos()), Mode: fieldMode(sel, stack, info, getters[sel])})
					}
				} else if _, ok := info.Uses[sel.Sel].(*types.Func); ok {
					mode := "unknown"
					if len(stack) > 0 {
						if call, ok := stack[len(stack)-1].(*ast.CallExpr); ok && call.Fun == sel {
							mode = fieldMode(call, stack[:len(stack)-1], info, false)
						}
					}
					out.Uses = append(out.Uses, Use{Pos: pos(sel.Sel.Pos()), Mode: mode})
				}
			}
			if call, ok := n.(*ast.CallExpr); ok {
				if id, ok := call.Fun.(*ast.Ident); ok {
					if _, ok := info.Uses[id].(*types.Func); ok {
						out.Uses = append(out.Uses, Use{Pos: pos(id.Pos()), Mode: fieldMode(call, stack, info, false)})
					}
				}
			}
			stack = append(stack, n)
			return true
		})
		result = append(result, out)
	}
	if err := json.NewEncoder(os.Stdout).Encode(result); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
