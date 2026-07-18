# agnes-registry

Owns the three namespaces agnes cares about — **types**, **type aliases**,
and **tools** — and enforces that every name is unique across type + alias
declarations. Resolves syntactic `TypeExprAst` into canonical `TypeExpr`.

## Public API

```rust
pub struct Registry { /* ... */ }

impl Registry {
    pub fn new() -> Self;

    pub fn register_type (&mut self, name: &str, v: Option<Validator>) -> Result<(), RegistryError>;
    pub fn register_alias(&mut self, name: &str, expr: TypeExpr)       -> Result<(), RegistryError>;
    pub fn register_tool (&mut self, name: &str, sig: ToolSignature)   -> Result<(), RegistryError>;
    pub fn override_tool (&mut self, name: &str, sig: ToolSignature);

    pub fn resolve (&self, ast: &TypeExprAst) -> Result<TypeExpr, RegistryError>;
    pub fn validator_of  (&self, ty: &TypeName) -> Option<Validator>;
    pub fn tool_signature(&self, name: &str)    -> Option<&ToolSignature>;

    pub fn load(&mut self, program: &Program) -> Result<(), RegistryError>;
}

pub fn defines_of(program: &Program) -> Vec<&TopLevel>;
```

`RegistryError` variants:

- `NameConflict { name, existing_kind, attempted_kind }` — a name is
  already registered under a different `EntryKind`.
- `UnknownName { name }` — a `TypeExprAst::Named` referenced a name that
  wasn't declared.

Both render as What / Why / Fix suggestion messages.

## What `resolve` does

- `Named` referring to a type → `TypeExpr::Named(TypeName)`.
- `Named` referring to an alias → the alias's stored `TypeExpr` (already
  canonical).
- `Named` referring to nothing → `UnknownName`.
- `Union` → recursively resolves every member, unions their sets. If the
  result collapses to a single member, it's returned as `Named`.

This is where "TypeScript-style union" becomes "flat `HashSet` for
membership tests".

## `Registry::load` semantics

- Applies every `DeclareType`, `DeclareTypeAlias`, and `DeclareTool`
  top-level to the registry, in file order.
- Skips `Define` — the compiler handles those (so cyclic defines are
  caught by cycle detection, not silently registered).
- Existing tool signatures may be *overridden* by a re-declare (letting
  the user override a built-in), but duplicate types/aliases are rejected.

## Pipeline position

```
agnes-ast::Program
     ↓  agnes-registry::load    ← you are here
Registry (types + aliases + tools)
     ↓  agnes-checker consults it
     ↓  agnes-compiler resolves tool signatures via it
     ↓  agnes-runtime uses validator_of for boundary checks
```

## Design notes

- Types and aliases share a namespace (both are typing "names"); tools
  live in a separate namespace (kebab-case, unlikely to collide with
  PascalCase types) but are still checked for duplicate registration on
  first declare.
- Aliases are resolved and stored eagerly. `resolve` never needs to walk
  the alias graph — one level of lookup is always enough.
