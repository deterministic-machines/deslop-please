# Proposal: Go frontend using SCIP

This is a design proposal, not an implementation of Go support. Its goal is to
apply deslop-please's architectural checks to Go code while reusing the existing
lint engine where its assumptions hold.

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
