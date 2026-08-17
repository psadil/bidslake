"""The file handle yielded by `BidsLake.get`."""

from __future__ import annotations

import dataclasses
from pathlib import Path
from typing import TYPE_CHECKING, Any

from upath import UPath

from .paths import to_local_path, to_upath

if TYPE_CHECKING:
    import polars as pl

    from .layout import BidsLake
    from .relations import Relation

# Columns present on every file-based row that are not BIDS concepts; excluded
# from the ``entities`` mapping.
#
# ``projected`` is the stored term-map projection backing the generated concept
# columns (ADR 0002). It is the *source* of some of this row's entities, not an
# entity itself, and it only exists on catalogs built with a term map — so leaving
# it in would surface a JSON blob as ``f.entities["projected"]`` on adapter
# catalogs and nowhere else.
_NON_CONCEPT = frozenset(
    {
        "file_id",
        "dataset_id",
        "root_uri",
        "file_path",
        "kind",
        "status",
        "other_data",
        "HED",
        "acq_time",
        "row_idx",
        "projected",
    }
)


@dataclasses.dataclass(frozen=True, slots=True)
class BidsFile:
    """A single data file, with its resolved location and BIDS concepts.

    Attributes:
        root_uri: Which of the dataset's ingest roots `file_path` is relative to
            (docs/adr/0005).
        file_id: The registry key every file-keyed table stores (docs/adr/0006).
        entities: Every non-null BIDS concept column of the row — the entities (`sub`,
            `task`, `run` …) plus `datatype`/`suffix`/`extension`/`modality`/`pseudofile`.
            The common ones are also exposed as properties.
    """

    dataset_id: str
    root_uri: str
    file_path: str
    file_id: int
    uri: str
    entities: dict[str, Any]
    # Back-reference to the opened database, for lazy metadata/events/associated
    # lookups. Excluded from equality/repr; not part of the file's identity.
    lake: BidsLake | None = dataclasses.field(default=None, compare=False, repr=False)

    @property
    def path(self) -> UPath:
        """One handle that opens/globs the file, local or remote."""
        return to_upath(self.uri)

    @property
    def local_path(self) -> Path:
        """The on-disk path, `file://` only.

        Raises:
            RemotePathError: For a remote URI, which has no on-disk path.
        """
        return to_local_path(self.uri)

    @property
    def sub(self) -> str | None:
        """The subject label with the `sub-` prefix stripped; None for a dataset-level file."""
        return self.entities.get("sub")

    @property
    def ses(self) -> str | None:
        """The session label; None where the name has no `ses-`, as in a sessionless dataset."""
        return self.entities.get("ses")

    @property
    def task(self) -> str | None:
        """The task label; None on files carrying no `task-` entity, such as anatomicals."""
        return self.entities.get("task")

    @property
    def run(self) -> str | None:
        """The run label as text, not an integer: `run-01` reads `01`, and sorts that way."""
        return self.entities.get("run")

    @property
    def suffix(self) -> str | None:
        """What the file is, read off the stem's last `_` segment (`bold`, `T1w`, `events`)."""
        return self.entities.get("suffix")

    @property
    def datatype(self) -> str | None:
        """Position, not the filename: the parent directory, when it names a datatype."""
        return self.entities.get("datatype")

    def __repr__(self) -> str:
        """Dataset and path only — enough to identify the file without printing every entity."""
        return f"BidsFile(dataset_id={self.dataset_id!r}, file_path={self.file_path!r})"

    # -- lazy lookups (require the lake back-reference) --------------------

    @property
    def metadata(self) -> dict[str, Any]:
        """The merged JSON-sidecar metadata, keyed by BIDS field name (`RepetitionTime`, …).

        An empty dict if the file has no sidecar.
        """
        return self._require_lake()._sidecar_metadata(self.file_id)

    def get_events(self) -> pl.DataFrame:
        """The task-event rows associated with this file.

        BIDS inheritance is already resolved at ingest. An empty frame if there are none.
        """
        return self._require_lake()._events_for(self.file_id)

    def get_described_by(
        self,
        kind: str,
        *,
        table: str | None = None,
        order_by: str | None = "row_idx",
    ) -> pl.DataFrame:
        """The rows of the file that *describes* this one, reached through `file_associations`.

        A confounds timeseries, a channels table, a gradient set. The rows are stored once
        against the describing file and shared by every file it describes, so this is the read
        side of BIDS inheritance for tabular companions: a root-level `dwi.bval` or
        `task-*_events.tsv` answers here for every image below it, without a copy per image.

        For the ordered per-volume tables an equivalent view exists and carries the BIDS
        concepts (`lake.get(table="diffusion", sub="01")`); this is the per-file lookup.

        Args:
            kind: The `association_type` of the edge to follow, which also names the table
                the rows are read from unless `table` overrides it.
            table: Read the rows from this table instead of the one `kind` names.
            order_by: Column to sort by; `None` leaves the rows in whatever order the join
                produces. The default, `row_idx`, is the describing file's own row order.

        Returns:
            An empty frame when the catalog has no such table — an adapter's table is absent
            from a catalog built without that adapter, which is an answer, not an error.
        """
        return self._require_lake()._rows_for(self.file_id, kind, table=table, order_by=order_by)

    def get_associated(self, kind: str | None = None) -> list[BidsFile]:
        """Files cross-referenced from this one (fieldmaps, events, …) via `file_associations`.

        Args:
            kind: One `association_type` to filter to. `None` returns every kind.
        """
        return self._require_lake()._associated_for(
            self.dataset_id, self.root_uri, self.file_id, kind
        )

    def related_datasets(self, relation: Relation | str | None = None) -> list[str]:
        """The dataset ids related to *this file's* dataset by explicit provenance.

        `BidsLake.related_datasets` for this file's `dataset_id`. A shared-source relation
        makes an entity match into the returned dataset(s) sound.

        Args:
            relation: One kind of relation to filter to. `None` returns every kind.
        """
        return self._require_lake().related_datasets(self.dataset_id, relation)

    def _require_lake(self) -> BidsLake:
        if self.lake is None:
            raise RuntimeError(
                "this BidsFile has no database reference; construct it via BidsLake.get()"
            )
        return self.lake

    @classmethod
    def _from_row(
        cls,
        dataset_id: str,
        root_uri: str,
        file_path: str,
        file_id: int,
        uri: str,
        row: dict[str, Any],
        lake: BidsLake | None = None,
    ) -> BidsFile:
        entities = {k: v for k, v in row.items() if k not in _NON_CONCEPT and v is not None}
        return cls(
            dataset_id=dataset_id,
            root_uri=root_uri,
            file_path=file_path,
            file_id=file_id,
            uri=uri,
            entities=entities,
            lake=lake,
        )
