"""Cross-dataset relation kinds (see the `dataset_relations` view / docs/adr/0003)."""

from __future__ import annotations

import enum


class Relation(enum.StrEnum):
    """How one dataset relates to another in a bidslake catalog.

    Deliberately not "sibling" — in DataLad a *sibling* is a remote/clone of the *same*
    dataset, whereas these relate *different* datasets by shared provenance.
    """

    #: Two datasets are co-derivatives of one source (they declare the same
    #: ``SourceDatasets``). The common case, and sound even when the shared source
    #: dataset is not itself in the catalog.
    SHARES_SOURCE = "shares_source"
    #: This dataset was derived from another dataset that is present in the catalog.
    DERIVED_FROM = "derived_from"
    #: The inverse of ``DERIVED_FROM``: another present dataset was derived from this one.
    SOURCE_OF = "source_of"
