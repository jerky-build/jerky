# Choosing an npm-flavoured semver crate

**Date:** 2026-09-08
**Status:** Answered
**Question:** Design §12, "An npm-flavoured semver crate for spec 2"
**Blocks:** #6 (semver range parsing), and therefore the spec 2 design

## The question

The design parked this as an open question and made it a gate:

> **An npm-flavoured semver crate for spec 2.** Rust's well-known `semver` crate
> is Cargo-flavoured, and npm's range semantics differ in real ways — `^`
> behaviour on `0.x`, `x`/`*` wildcards, hyphen ranges, `||` unions. An
> npm-compatible implementation is needed; which crate, and whether it is sound,
> must be verified before spec 2 commits to one.

"Whether it is sound" is the operative half. Every candidate's own description
claims node-semver compliance, so descriptions cannot separate them.

## Method

Differential testing against the reference implementation rather than against a
reading of the spec.

npm's own `semver` package is the oracle. It has zero dependencies, which makes
it exactly jerky's spec 1 target case — so it was installed with jerky itself:

```
$ jerky init && jerky install semver
added semver@7.8.5
```

**531 ranges × 31 versions.** Ranges are generated combinatorially over the
operators (`^ ~ >= > <= < =` and bare), partial and full versions, prerelease
and build-metadata tails, plus wildcards, hyphen ranges, comparator sets,
`||` unions, and the whitespace and `v`-prefix oddities npm tolerates. For each
range the oracle records whether npm considers it valid at all, and exactly
which versions satisfy it.

64 of the 531 are ranges npm **rejects**. Those matter as much as the valid
ones: a crate that scores well by accepting everything is not compliant, it is
permissive. Agreeing with the oracle means rejecting those too.

A candidate scores a range only when it agrees with npm completely — same
validity verdict, and byte-identical satisfying sets.

## Results

| Crate | Score | Crates pulled | License |
|---|---|---|---|
| **js-semver 0.4.0** | **531/531** | **2** | MIT-0 |
| nodejs-semver 5.0.0 | 515/531 | 16 | Apache-2.0 |
| node-semver 2.2.0 | 513/531 | — | Apache-2.0 |
| semver 1.0.28 (Cargo) — control | 431/531 | — | MIT/Apache-2.0 |

### The control confirms the design's hypothesis, and sharpens it

Cargo's `semver` fails 100 of 531. The design predicted incompatibility; what
it did not say is how the incompatibility presents. **55 failures are refusals
to parse. The other 45 are silent wrong answers.**

Refusals are the safe half, and they are not exotic syntax:

```
>=1.2.3 <2.0.0        REFUSED — a comparator set, among the most common npm ranges
1.2.3 - 2.3.4         REFUSED — hyphen range
^1.0.0 || ~2.1.0      REFUSED — union
```

The silent half is the reason this question was worth gating on. Cargo's
prerelease rules differ from npm's, and the disagreement produces answers, not
errors:

```
range "0.0.0-alpha"   wrongly included ["0.0.0"]
range "0.0.3-alpha"   wrongly included ["0.0.3"]
range "1.2.3-alpha"   wrongly included ["1.2.3", "1.2.4", "1.3.0", "1.9.9",
                                        "1.2.3-alpha.1", "1.2.3-beta.2", ...]
```

A resolver built on this would not fail loudly on a prerelease range. It would
install a different version than npm installs, and the lockfile would record
that divergence as though it were correct.

### Separating the two real candidates

At 26 ranges both `js-semver` and `nodejs-semver` scored perfectly; the wider
sweep separated them.

All 16 `nodejs-semver` failures are **one defect repeated across operators**:
build metadata on a partial version.

```
1.2+build.5   ^1+build.5   >=1.2+build.5   <1+build.5   ...
```

It refuses to parse these; npm accepts them. This is narrow — build metadata
inside a *range* is already rare, and on a partial version rarer still — and it
fails loudly rather than answering wrongly. It is a real gap, not a dangerous
one.

`js-semver` agreed with the oracle on every range, including rejecting all 64
that npm rejects.

## Recommendation

**Use `js-semver`.** It is the only candidate with no known divergence from the
reference implementation, and it is also the lightest by a wide margin — 2
crates against `nodejs-semver`'s 16, which matters for a tool whose whole
dependency tree is 75.

### The honest risk

`js-semver` is **0.4.0**. A pre-1.0 crate can break its API on a minor bump,
and it is a newer and less-used project than `nodejs-semver`. Two mitigations,
both cheap and both worth building into the spec 2 design:

1. **Wrap it behind jerky's own range type.** The same move the design already
   made for `RegistryClient` — "the trait matters more than the client". A
   swap then touches one module rather than the resolver.
2. **Keep this conformance suite in the repo, not in a scratch directory.**
   It is the actual insurance. If `js-semver` has to be replaced, the suite
   decides whether the replacement is acceptable in one run, rather than
   re-litigating this question from scratch. `nodejs-semver` is the fallback,
   and its single known gap is documented above.

## Reproducing

The oracle generator, run in a directory where `jerky install semver` has been
run:

```js
// gen.js — writes oracle.json and versions.json
const s = require('semver');
const ops   = ["", "^", "~", ">=", ">", "<=", "<", "="];
const bases = ["0.0.0","0.0.3","0.1.0","0.2.3","1.0.0","1.2.3","1.2","1","2.0.0","2.3.4"];
const tails = ["", "-alpha", "-alpha.1", "-beta.2", "-rc.1", "+build.5"];

const ranges = new Set();
for (const o of ops) for (const b of bases) for (const t of tails) ranges.add(o + b + t);
for (const w of ["*","x","X","1.x","1.X","1.2.x","0.x","*.*.*","1.*","1.2.*"]) ranges.add(w);
for (const a of ["1.2.3","1.2","1","0.2.3"]) for (const b of ["2.3.4","2.3","2","1.2.4"]) ranges.add(`${a} - ${b}`);
for (const a of [">=1.2.3","^1.0.0","~1.2"]) for (const b of ["<2.0.0","<=2.3.4",">0.1.0"]) {
  ranges.add(`${a} ${b}`); ranges.add(`${a} || ${b}`);
}
for (const r of ["  ^1.2.3  ", "^ 1.2.3", ">= 1.2.3", "1.2.3||2.0.0", "v1.2.3", "^v1.2.3", "=v1.2.3"]) ranges.add(r);

const versions = [
  "0.0.0","0.0.1","0.0.3","0.0.4","0.1.0","0.1.1","0.2.3","0.2.4","0.3.0",
  "1.0.0","1.1.0","1.2.2","1.2.3","1.2.4","1.3.0","1.9.9","2.0.0","2.3.3","2.3.4","2.3.5","3.0.0",
  "1.2.3-alpha","1.2.3-alpha.1","1.2.3-alpha.2","1.2.3-beta.2","1.2.3-beta.3","1.2.3-rc.1",
  "2.0.0-rc.1","2.0.0-alpha","0.2.3-beta.1","1.2.4-alpha.1",
];

const out = [];
for (const r of ranges) {
  let valid = true;
  try { new s.Range(r); } catch (e) { valid = false; }
  const sat = [];
  if (valid) for (const v of versions) { try { if (s.satisfies(v, r)) sat.push(v); } catch (e) {} }
  out.push({ range: r, valid, satisfying: sat });
}
require('fs').writeFileSync('versions.json', JSON.stringify(versions));
require('fs').writeFileSync('oracle.json', JSON.stringify(out));
```

The comparison harness is a throwaway binary depending on all four crates, each
wrapped to the same shape: parse the range, return `None` if it refuses,
otherwise return the filtered version list. A candidate is correct on a range
when `(parsed, satisfying)` matches the oracle's `(valid, satisfying)` exactly.

```rust
type Answer = Option<Vec<String>>;

fn js_semver(range: &str, versions: &[String]) -> Answer {
    let r = js_semver::Range::parse(range).ok()?;
    Some(versions.iter()
        .filter(|v| js_semver::Version::parse(v).map(|pv| r.satisfies(&pv)).unwrap_or(false))
        .cloned().collect())
}
// nodejs_semver, node_semver and the Cargo `semver` control differ only in
// the paths and, for Cargo, `VersionReq::parse` / `matches` in place of
// `Range::parse` / `satisfies`.
```

## What this unblocks

Design §12's open question is answered, so the spec 2 design can be written.
#6 and #11 remain one design rather than two, for the reason #11 gives: a
lockfile's format is the serialization of the resolver's output type.
