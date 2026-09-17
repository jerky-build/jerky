# The Config File — Design

**Date:** 2026-09-16
**Status:** Approved
**Issues:** #12 (this spec)
**Blocks:** #132, and #41 when it is picked up

## 1. Context: one issue asking for two things

#12 asks for "a basic json config file" that can "describe a project and its
targets". That is the foundation of the task runner — #8, #9, #13, #14, #15,
#16, #17 and #18 all read it eventually. Since it was filed, something else has
come to need a configuration surface for a completely different reason:
`docs/specs/2026-09-16-lifecycle-scripts-design.md` §4 puts the build allowlist
here, and #41 would put single-version enforcement here too.

**Those two have opposite readiness, and this spec designs only one of them.**

Install policy is ready. Its shape is settled by the spec that needs it: a list
of package names, read at the workspace root, loadable before the first
dependency is materialised. It is blocking #132 today.

A target schema is not ready, and writing one now would mean designing against
zero usage. `jerky build` is #8 and is unimplemented; nothing in the repository
runs a task, caches one, or orders two. Whatever this document said about
targets, inputs, outputs and cross-project dependencies would be invented rather
than observed, and would be load-bearing for four issues by the time anyone
found out it was wrong.

So this spec settles **the container** completely — its name, where it lives,
how it is discovered, how it relates to `package.json`, what happens to a key
jerky does not recognise — and fills it with the one section that is ready.
§7 says where targets go and why the container is what that spec needs from
this one.

## 2. Settled decisions

1. **A file of jerky's own, `jerky.json`, at the workspace root.** §3.
2. **Install policy only.** The build allowlist. Targets are §7.
3. **Strict JSON**, no comment syntax and no new parsing dependency. §4.
4. **The allowlist accepts a bare name or a `{name, reason}` object**, mixed
   in one array. §5.
5. **Strict validation**: unknown keys, duplicate names and implausible names
   are all refused. §6.
6. **jerky never writes this file.** §3.

## 3. The file

`jerky.json`, in the workspace root — the directory holding the root
`package.json`, the same root the lockfile sits in.

```
my-app/
  package.json        npm's file. jerky writes dependencies into it.
  jerky.json          jerky's file. jerky only ever reads it.
  jerky-lock.json     jerky's output.
  packages/ui/package.json
```

### Why not a key in `package.json`

The ecosystem has moved away from it and the reasons apply here. pnpm left
`package.json` for `pnpm-workspace.yaml`, yarn has `.yarnrc.yml`, turbo has
`turbo.json`, nx has `nx.json`.

The jerky-specific reason is stronger than precedent. `Manifest::save`
reserialises `package.json` on every install that adds a dependency. Config
living there would be config jerky rewrites — #24 is already open about that
rewrite losing a file's indentation — and a security-relevant allowlist is the
worst possible payload for a reserialisation bug.

Keeping them apart also keeps the ownership clean in a way worth stating
plainly: **`package.json` is a file jerky writes to and `jerky.json` is a file
jerky reads from.** Nothing in jerky ever writes `jerky.json`. A future
`jerky config allow <pkg>` would be a change to that rule and would have to
argue for it, rather than inheriting it by accident.

### The root, and only the root

Install policy is workspace-wide by nature. There is one content store, one
install, one lockfile, and a dependency's build script runs once for a tree
rather than once per member that depends on it. A per-member allowlist would be
asking which member's opinion wins about a package they share.

**A `jerky.json` in a workspace member is an error**, and the error names the
file. The alternative — ignoring it — is worse than it sounds: a user writes an
allowlist, sees no effect, and learns that jerky ignores files. Refusing is
also the reversible direction. When the targets spec gives a member-level file
a meaning, it removes an error; had this spec ignored the file, that spec would
be changing behaviour users had come to rely on.

### A missing file is a valid empty config

`jerky.json` is optional. Its absence is not a warning and not a prompt; it is
a workspace that has allowed nothing, which is the default posture anyway.

This has to be true rather than merely convenient. The lifecycle spec's
deny-by-default only means something if denying is what happens when nothing is
configured — a safe default that depends on a file existing is not a default.

## 4. Strict JSON

Parsed with `serde_json`, which the crate already depends on. No JSONC, no
JSON5, no comment stripping, and therefore no new dependency.

The cost is real: there is nowhere to write a comment, and the one thing in
this file that people genuinely need to annotate is the allowlist — which is a
record of who has been granted permission to execute code on the machine, and
six months later somebody has to know whether `sharp` is in there because it is
needed or because a CI run was broken at 2am.

§5 answers that inside the data rather than beside it, which is better than a
comment in two respects: `jerky` can print the reason back in its own output,
and the reason survives any tool that reads and rewrites the file.

## 5. `allowedScripts`

```json
{
  "allowedScripts": [
    "sharp",
    { "name": "@parcel/watcher", "reason": "file watching; reviewed 2026-09-16" }
  ]
}
```

An array whose entries are each **either** a bare package name **or** an object
with `name` and `reason`. The two shapes mix freely in one array.

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum AllowedScript {
    /// `"sharp"` — allowed, with no reason recorded.
    Bare(String),
    /// `{"name": "sharp", "reason": "..."}`.
    Explained { name: String, reason: String },
}
```

`#[serde(untagged)]` is the idiom `binaries::Declared` already uses for npm's
two `bin` shapes, so this is the crate's existing answer to "one field, two
spellings" rather than a new one.

**Why both shapes.** The bare form is what a user will type and what every
other tool's allowlist looks like; demanding a reason for each entry would be a
tax on the common case and would be paid in empty strings. The object form is
there because the annotation belongs in the file that grants the permission,
and a reason recorded beside the grant is a reason a reviewer sees in the diff.

**`reason` is required in the object form.** An object is what a user reaches
for in order to say something; `{"name": "sharp"}` is the bare form with extra
punctuation, and accepting it would mean two spellings of one thing.

That decision has a cost this spec has to name, because `#[serde(untagged)]`
pays it: an entry matching neither variant produces *"data did not match any
variant of untagged enum"*, which tells a user nothing about the line they got
wrong. A file this small and this security-relevant cannot have an error like
that. The implementation owes a hand-written message naming the entry's index
and what the two shapes are — which is what `binaries::deserialize_lenient`
avoids having to do by being lenient, an option §6 rules out here.

`reason` is free text. jerky does not parse it, and prints it verbatim in the
report the lifecycle spec §4 describes, so a skipped-package listing can say
what a previously-allowed package was allowed *for*.

**`name` is the whole of the grant.** Not a range, not a pattern. The lifecycle
spec §4 argues both: a range invites the belief that the allowlist is a
boundary against a version, which it is not — the lockfile pins versions and
this is a statement about a publisher — and a pattern invites allowlisting a
namespace that anyone can publish into.

## 6. Validation, and why it is strict here

**Everything below is refused with an error naming the problem.** That is the
opposite of how jerky reads `bin` off a packument, and the contrast is the
point rather than an inconsistency.

`crate::binaries::deserialize_lenient` is deliberately forgiving because its
blast radius is enormous: `Packument::versions` holds every published version
in one map, so a version serde refuses takes the whole package down, at every
version, for every project that depends on it. Against that, ignoring one
malformed `bin` costs a missing shim.

`jerky.json` inverts every term. It is one file, in one repository, written by
the person running the command, and the cost of quietly ignoring part of it is
a permission the user believes they granted and did not — or a package they
believe they allowed that is silently still denied. Leniency at the registry
boundary and strictness at the user boundary are the same judgement applied to
opposite inputs.

So:

- **An unrecognised top-level key is an error.** `allowedScript` without the
  `s` would otherwise be a file that looks configured and denies everything.
  This also means a `jerky.json` written for a future jerky fails on today's —
  see §7, which is why that is the right side to err on for now.
- **A duplicate name is an error**, whichever shapes the two entries take.
  Two grants for one package with two different reasons is a question with no
  answer, and picking one silently would make the file's meaning depend on its
  order.
- **A name that is not a plausible package name is an error.**
  `resolver::is_plausible_name` already owns that rule — it is what stops an
  `npm:` alias sending `@1.0.0` to the registry as a package name — and a
  second definition here would be a second thing to keep in agreement.
- **An entry naming a package the tree does not contain is a warning**, not an
  error. It is a permission granted to nothing, which is exactly what is left
  behind when a dependency is removed, and it should decay visibly rather than
  accumulate. A warning rather than an error because the tree changes with
  `--production`, with a platform, and with a `--filter` that does not exist
  yet: a grant that is unused today may be used tomorrow by the same file.

## 7. Targets, and what this spec owes that one

A later spec adds a `targets` key, and everything in this document is written
so that it can.

**Nothing is reserved in code.** Because §6 refuses unrecognised keys there is
nothing to reserve: the key does not exist until the spec that defines it adds
it, and until then a file carrying one is refused rather than half-understood.
Reserving a name jerky does not implement would be carrying a field with no
reader, which is the shape of speculative generality this repository keeps
turning down — #119 is deferred until someone will defend a detection method,
and #107 bought most of its value without touching `Kind`.

What that spec inherits is the container and the decisions in §3, §4 and §6: it
does not have to re-argue where the file lives, whether it takes comments, or
what happens to a key nobody recognises. It does have two questions this one
deliberately leaves open, because they are about targets rather than about the
file:

- **Whether a workspace member may have its own `jerky.json`.** §3 refuses one
  today. Install policy is workspace-wide, but targets are per-project by
  nature, so that spec is where the refusal is revisited. It is removing an
  error, which is why refusing now is safe.
- **Whether refusing unknown keys survives contact with a schema that
  evolves.** It is right now: one implementation, pre-1.0, and a config jerky
  does not understand should not be silently half-applied. It is the decision
  here most likely to become annoying, and the honest trigger to revisit it is
  a second thing reading this file — a plugin, an editor, or a jerky old enough
  to meet a file newer than itself.

## 8. Scope

### In scope

- `jerky.json`, its location, and its discovery from the workspace root.
- A missing file as a valid empty config.
- Refusing a member-level `jerky.json`.
- `allowedScripts`, in both entry shapes.
- The validation rules in §6, and the warning for an unused grant.

### Explicitly out of scope

- **Targets, and everything reading them.** §7.
- **Single-version enforcement (#41).** It belongs in this file and its design
  is #41's — what it needs from here is that the container exists and that a
  new key can be added to it.
- **Any command that writes `jerky.json`.** §3 makes not writing it a property
  rather than an omission.
- **Environment or CLI overrides of the allowlist.** A `--allow-scripts` flag
  was considered when the lifecycle spec chose its default and left out, on the
  grounds that it is a flag people paste into CI permanently. Nothing here
  reopens that.
- **A schema file.** Worth having once there is more than one key worth
  completing.

## 9. Testing

- A workspace with no `jerky.json` loads, and allows nothing.
- Both entry shapes parse, mixed in one array, and produce the same grant.
- A `reason` survives to wherever the lifecycle report can print it.
- An unknown top-level key is refused, and the error names the key. The typo
  case — `allowedScript` — is the test worth writing by name.
- A duplicate name is refused whichever shapes the two entries take.
- An implausible name is refused.
- A grant naming a package the tree does not contain warns and does not fail.
- **A `jerky.json` in a member is refused, and the error names the file.** Per
  the invariants this is also the test that exercises more than one importer.
- Nothing jerky does rewrites `jerky.json`: a file with unusual whitespace and
  key order is byte-identical after an install that adds a dependency.
