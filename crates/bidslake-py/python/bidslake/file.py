"""The file handle yielded by :meth:`BidsLake.get`."""

from __future__ import annotations

import dataclasses
from pathlib import Path
from typing import TYPE_CHECKING, Any

from upath import UPath

from .paths import to_local_path, to_upath

if TYPE_CHECKING:
    import polars as pl

    from .layout import BidsLake

# Columns present on every file-based row that are not BIDS concepts; excluded
# from the ``entities`` mapping.
_NON_CONCEPT = frozenset({"dataset_id", "file_path", "other_data", "HED", "acq_time", "row_idx"})


@dataclasses.dataclass(frozen=True, slots=True)
class BidsFile:
    """A single data file, with its resolved location and BIDS concepts.

    ``entities`` holds every non-null BIDS concept column of the row — the
    entities (``sub``, ``task``, ``run`` …) plus ``datatype``/``suffix``/
    ``extension``/``modality``/``pseudofile``. The common ones are also exposed
    as attributes for convenience.
    """

    dataset_id: str
    file_path: str
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
        """The on-disk path (``file://`` only; raises for remote URIs)."""
        return to_local_path(self.uri)

    @property
    def sub(self) -> str | None:
        return self.entities.get("sub")

    @property
    def ses(self) -> str | None:
        return self.entities.get("ses")

    @property
    def task(self) -> str | None:
        return self.entities.get("task")

    @property
    def run(self) -> str | None:
        return self.entities.get("run")

    @property
    def suffix(self) -> str | None:
        return self.entities.get("suffix")

    @property
    def datatype(self) -> str | None:
        return self.entities.get("datatype")

    def __repr__(self) -> str:
        return f"BidsFile(dataset_id={self.dataset_id!r}, file_path={self.file_path!r})"

    # -- lazy lookups (require the lake back-reference) --------------------

    @property
    def metadata(self) -> dict[str, Any]:
        """The merged JSON-sidecar metadata for this file, keyed by BIDS field
        name (`RepetitionTime`, …). Empty dict if the file has no sidecar."""
        return self._require_lake()._sidecar_metadata(self.dataset_id, self.file_path)

    def get_events(self) -> pl.DataFrame:
        """The task-event rows associated with this file (BIDS inheritance already
        resolved at ingest). Empty frame if there are none."""
        return self._require_lake()._events_for(self.dataset_id, self.file_path)

    def get_associated(self, kind: str | None = None) -> list[BidsFile]:
        """Files cross-referenced from this one (fieldmaps, events, …) via
        `file_associations`, optionally filtered to one `association_type`."""
        return self._require_lake()._associated_for(self.dataset_id, self.file_path, kind)

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
        file_path: str,
        uri: str,
        row: dict[str, Any],
        lake: BidsLake | None = None,
    ) -> BidsFile:
        entities = {k: v for k, v in row.items() if k not in _NON_CONCEPT and v is not None}
        return cls(
            dataset_id=dataset_id, file_path=file_path, uri=uri, entities=entities, lake=lake
        )
