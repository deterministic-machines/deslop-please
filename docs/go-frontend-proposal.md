# Proposal: Go frontend using SCIP

The Go frontend indexes Go modules with scip-go, type-checks their packages
from source, and feeds a Go model to the existing lint engine. It remains
experimental. The section below describes what is implemented; the rest of
this document is the original design and its open questions.

## As implemented

**Workspace.** The go command chooses it: the module above the current
directory, or the `go.work` it selects (also through `GOWORK`). Each main
module is indexed by scip-go on its own, wherever it is; paths are relative to
the workspace's directory, through `..` for a module outside it. Build tags,
`GOOS`, `GOARCH` and `GOFLAGS` select the files for both the index and the type
checker. Test files are analyzed and marked as test code.

**Model.** Each Go package is a package, named by its import path; an external
test package is `path_test`. Types (aliases included), functions, methods,
struct fields (also of nested anonymous structs), interface methods, and
package-level constants and variables are items. A method belongs to the type
it is declared on, through an alias. A constant of a type its own package
declares is one of that type's values, as a Rust enum's variant is, so an enum
and the `String` method that names its values make no tangle. Types declared
inside functions are not items. scip-go gives the references by name; the
analyzer (`facts.go`) adds
what the code does with each use, the accessors and forwarders, and the
references the source does not spell out.

**Reads.** A use of a field, or of what a getter returns, only reads when it
cannot write the storage or keep a way to it: arithmetic, comparisons,
`len`/`cap`, indexing or ranging that yields plain data, a value-receiver
method on plain data, or handing on a copy of plain data (no pointer, slice,
map, channel, function, interface or type parameter inside). Everything else
is a write: assignment, `++`, `&`, append, copy into, delete, clear, close,
channel operations, calling a function-valued field, a pointer-receiver
method, or handing on a value that shares storage. A struct literal gives
each field its value whole, as a Rust struct literal does, so it only reads.
Interface values count as sharing storage.

**Accessors and forwarders.** A method whose body only returns fields of its
receiver (or their addresses) is a getter; one that only assigns parameters to
them is a setter; a function whose body only calls another workspace function
with all its parameters, possibly on a field of the receiver, is a forwarder.
A forwarder to an interface method is not one: the call may run anything.

**Implicit references.** A promoted field's use is also a use of the embedded
fields it is reached through. A struct literal without keys names each field.
A call of an interface's method, directly, through an embedded interface, or
on a type parameter, is also a *dispatched* use of each workspace method it
may run: the methods of the concrete types that implement the interface among
those the workspace's non-test code converts to an interface (assignment,
argument, result, literal element, send, map key, explicit conversion) or
passes as a type argument. Dispatched uses count for `field-leak`, so a getter
reached through an interface still exposes its field, but not for `tangle`,
`module-cycle` or `shallow-module`: calling through an interface is not
knowing the implementation.

**Limitations.**

- Dynamic types made by reflection or by conversions inside dependencies are
  not seen; a function value is not followed to the function it holds.
- A call of a promoted method is not a use of the embedded field it goes
  through: embedding publishes the embedded type's methods.
- Methods that implement interfaces are counted like any other by
  `shallow-module`; Go has no `impl` blocks to tell them apart.
- Packages that use cgo cannot be analyzed: scip-go indexes cgo's generated
  files instead of the sources. Code that does not type-check is rejected.
- Generated files are analyzed like any other.
- Only the workspace's main modules are analyzed, not their dependencies.

## Approach

1. Discover Go modules and packages, including workspace, build-tag, platform,
   and test configurations.
2. Run [scip-go](https://github.com/scip-code/scip-go) to index the selected
   configuration. Translate its definitions and references into the model used
   by the graph and lint engine.
3. Supplement the index with Go source and type analysis where it does not
   provide the facts a rule requires. Compare actual SCIP output with the model
   before choosing how to extract each fact.
4. Adapt the model and rules where Rust-specific concepts do not map to Go;
   retain existing Rust behavior and report unsupported or uncertain cases
   rather than presenting guesses as facts.

SCIP is an index format, not a set of lint rules. Running a different indexer
alone does not produce the semantic model the existing engine needs. In
particular, field accesses, copies, mutable aliases, accessors, forwarding
methods, and interface dispatch need Go-specific treatment. Go's package,
module, and visibility semantics also differ from Rust's.

## Rule considerations

- **module-cycle:** Go rejects package import cycles already. Decide whether the
  existing rule should detect cycles among logical components or directory
  groups; avoid duplicating compiler errors.
- **tangle:** Build dependencies at appropriate Go package or component
  boundaries, then evaluate whether the existing graph rule still applies.
- **field-leak:** Analyze struct fields and uses across ownership boundaries,
  including pointers, slices, maps, embedded fields, and accessor methods.
- **shallow-module:** Define Go-appropriate interface and implementation
  boundaries, accounting for exported identifiers and implicit interfaces.
- **layout checks:** Apply existing file and directory checks to Go files with
  explicit treatment of generated files and tests.

## Validation and open questions

Begin with small Go fixtures and inspect the resulting SCIP indexes against the
model's requirements. Add end-to-end fixtures for each supported rule, including
non-findings, build tags, tests, generated code, multi-module workspaces, and
interface dispatch. Verify that Rust fixtures and diagnostics remain unchanged.

Questions for maintainers:

- What should count as a module boundary in a Go project?
- Which build configurations and test packages should be analyzed by default?
- Which semantic facts can be derived reliably from SCIP and which require Go
  tooling? How should unresolved dynamic behavior be presented?
- Should generated code be analyzed by default?

An AI agent could optionally help explain findings and suggest refactorings with
source evidence. It would not be required for deterministic lint results.
