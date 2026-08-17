# Architecture decisions

These records explain why bidslake is shaped the way it is. Read one when you want to change
something and need to know what a constraint is protecting.

Each follows the [PEP 12](https://peps.python.org/pep-0012/) template, reduced to the sections
that carry weight for a design record: Abstract, Motivation, Rationale, Specification, Backwards
Compatibility, Rejected Ideas, Open Issues. Start a new one from [`template.md`](template.md).

PEP's *Reference Implementation* section is deliberately not among them. It exists to record
whether a proposal has been built yet; a record here describes a system that already works, so
the whole document is about the implementation. An inventory of the functions involved would only
be a list nothing verifies, going stale at the first rename. Where a specific symbol is load-bearing
the record names it inline, in the sentence that needs it.

## Two conventions worth knowing before you read

**Every record describes how the system works now.** There are no dated amendments, no
"superseded by" banners, and no account of what an earlier draft said. When a decision changes,
the record is rewritten and git holds the history. So a record never tells you what bidslake
used to do — only what it does, and why that was chosen over the alternatives.

**Rejected Ideas is load-bearing.** It is where the alternatives that were seriously considered
are recorded with the reason they lost. It is usually the fastest way to find out whether an
idea you are about to propose has already been tried.

## The records

All are `Provisional` in the PEP sense — accepted and implemented, but the project is early
enough that further feedback is expected to change them.

| # | Title | What it settles |
| --- | --- | --- |
| [0001](0001-schema-augmentation-overlays.md) | Schema augmentation via additive overlays | How the vendored BIDS vocabulary gets extended to describe a derivative, without forking the schema. |
| [0002](0002-adapters-and-layouts.md) | Adapters and layouts: reading and writing a producer's tree | Both directions for a producer's directory — reading one into a catalog (overlay, term map, ingestion fragment) and naming a path in one before the file exists (layout, roles). |
| [0003](0003-associations.md) | Associations, within and across datasets | One shape for "this file describes that one", and how the same idea extends across `dataset_id`. |
| [0004](0004-undeclared-column-policy.md) | Storage is a policy, not an invariant | What happens to a column a table's schema does not declare. |
| [0005](0005-multi-root-datasets.md) | A dataset may span several ingest roots | Why `dataset_roots` exists and what stops a subject-sharded pipeline output from being N datasets. |
| [0006](0006-file-registry.md) | A real file registry, and a surrogate key for a file | `file_id`, `all_files`, and what every other table joins to. |
| [0007](0007-root-tenure.md) | Root tenure, and what the catalog is allowed to conclude | `attached` vs `managed` per root, and the limits of what an index may assert about files it does not own. |

## Dependencies

Only relationships that are still true today are recorded, in each record's `Requires:` header.
Nothing points at a record that no longer exists.
