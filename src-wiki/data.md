# Data

Coordinates, ingest and fixtures.

## Coordinates

[Invariants 1–3](../CONTRIBUTING.md#project-invariants--read-before-any-edit) are the rules;
`src/basepair/mod.rs` is the mechanism. Rejected alternatives:

- **A display-base toggle.** Display is always 1-based. The `CoordBase` setting was deleted
  (2026-08-06) because reading it threaded settings through the whole render tree for one field. If
  a toggle is ever wanted, it is a parameter on `to_display`/`from_display`.
- **A kind tag that affects arithmetic, or a separate coordinate type.** `BpKind` is read only by
  `to_display`; mixing a `Coord` and a `Length` in arithmetic is allowed. Ingest and display are
  enforced at the named conversion points instead.
- **`f64 → BasePair`.** Deliberately absent; `as_f64` is one-way.

## Fixtures

The [manifest](../testdata/README.md) lists every fixture and how to rebuild it.

- **Test BAMs are synthetic.** `src/testgen/` uses [aggregate HG002 calibration counts](../testdata/README.md#calibrating-the-synthetic-bam-generators)
  and synthesizes reads against the fixture reference. The HG002 rendering examples use the public
  ONT subset under its dataset license.
- **Each VCF fixture has one job; don't merge them.** `sample.vcf` holds the shapes a parser trips
  on, including the never-carried `UNUSED` key that the header-driven column test depends on.
  `raw_info.vcf` declares no `##INFO`, for the raw-blob fallback. `clinvar_subset.vcf.gz` is real
  data for `ex-vcf-clinvar` and no test needs it.
