# Introduction

This book is the explanatory half of bidslake's documentation: the roadmap, and the architecture
decision records that say why the system is shaped the way it is. It assumes you know what
[BIDS](https://bids-specification.readthedocs.io/) is and that you have written SQL before. It
does not assume you have used DuckDB.

It is deliberately not a tutorial — there is no step-by-step guide here yet. For getting
something running, the repository [`README.md`](https://github.com/psadil/bidslake#quickstart)
has the install and a first query, and `crates/bidslake-py/README.md` works through the Python
API end to end.

The API reference is not here. It is rustdoc, built with `cargo doc --open`, and the Python API
is documented in `crates/bidslake-py/README.md` and in the modules themselves.

## The idea

bidslake consolidates the metadata of a neuroimaging dataset — scattered across JSON sidecars,
`.tsv` tables and filename entities — into a single DuckDB catalog, while the bulky image files
stay exactly where they are. You query and edit the dataset with SQL, and when you need an image
you ask the catalog for its path.

```
     on disk                                        the catalog
     -------                                        -----------

  study/                                    +-- all_files -----------------+
    dataset_description.json  --parse-->    | file_id  sub  ses  task      |
    participants.tsv          --read--->    | datatype  suffix  kind       |
    sub-01/anat/..._T1w.json  --parse-->    | file_path  root_uri  status  |
    sub-01/anat/..._T1w.nii.gz --record->   +--------------+---------------+
              |                                            | file_id
              |                             +--------------+---------------+
              |                             | sidecars   events   physio   |
              |                             | participants   channels  ... |
              |                             +------------------------------+
              |
              +-- never moved, never opened; SQL hands you the path
```

Every table above is generated from the BIDS schema rather than hardcoded, which is why a
dataset that uses a modality bidslake has never been pointed at still lands in the right tables.

## Vocabulary

The ADRs use these words precisely. Each is owned by one record, linked here.

| Term | Meaning |
| --- | --- |
| **dataset** | A logical collection of files sharing a `dataset_id`. Many datasets can live in one catalog and be queried together. |
| **root** | An ingest root — the directory or `s3://` prefix a dataset was walked from. One dataset may span several ([ADR 0005](adr/0005-multi-root-datasets.md)). |
| **tenure** | Per root: `attached` (someone else writes there and bidslake only reads) or `managed` (bidslake owns the storage) ([ADR 0007](adr/0007-root-tenure.md)). |
| **file registry** | `all_files` — one row per file the walk saw, keyed by a surrogate `file_id` that everything else joins to ([ADR 0006](adr/0006-file-registry.md)). |
| **overlay** | An additive fragment extending the vendored BIDS vocabulary, so a derivative's entities and suffixes can be described ([ADR 0001](adr/0001-schema-augmentation-overlays.md)). |
| **term map** | A [BEP-043](https://bids.neuroimaging.io/extensions/beps/bep_043.html) document projecting a path onto BIDS concepts. The **read** direction ([ADR 0002](adr/0002-adapters-and-layouts.md)). |
| **ingestion schema** | bidslake's own rules deciding, per file, whether to read it, catalog it unread, or ignore it ([ADR 0002](adr/0002-adapters-and-layouts.md)). |
| **adapter** | The named bundle of overlay, term map and ingestion fragment that `--adapter` resolves ([ADR 0002](adr/0002-adapters-and-layouts.md)). |
| **layout**, **role** | The **write** direction: a document naming the slots in a tool's directory, so an output path can be named before the file exists ([ADR 0002](adr/0002-adapters-and-layouts.md)). |
| **association** | A recorded relationship between files, or between datasets ([ADR 0003](adr/0003-associations.md)). |

## Where to go next

- **[Architecture decisions](adr/index.md)** — why the system is shaped this way. Read these when
  you want to change something and need to know what a constraint is protecting.
- **`crates/bidslake-py/README.md`** — the Python query API, worked through end to end: opening a
  catalog, composing queries, resolving siblings, naming outputs through a layout.
- **`cargo doc --open`** — the API reference. The `bidslake` crate page carries runnable examples,
  and its `schema` module is the table-by-table database reference.
