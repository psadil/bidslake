"""Resolving dataset-relative file paths to openable handles.

`root_uri` (one row per ingest root in `dataset_roots` — a dataset may have
several; see `docs/adr/0005`) is `file:///abs/path` for a locally-ingested
dataset or `s3://bucket/prefix` for an S3 one. The join itself is done in Rust
(`_bidslake.resolve_uri`) so it matches exactly how the ingester formats those
URIs; this module wraps the result in a `upath.UPath` so callers get one
handle that works for local and remote alike.
"""

from __future__ import annotations

import os
from pathlib import Path

from upath import UPath


class RemotePathError(RuntimeError):
    """Raised when a local filesystem path is requested for a remote URI."""


def to_uri(location: str | os.PathLike[str]) -> str:
    """Normalize a filesystem path or URI to a URI (no trailing slash).

    Used to rebase `root_uri` when a dataset has moved (`open(..., base_dir=/root_override=)`).
    Also the write direction of `to_local_path`, and the thing to reach for when a query must
    be scoped to one tree — `WHERE root_uri = to_uri(dst)`. Scoping matters: a catalog holds
    many roots, and a completion query that names only a `dataset_id` answers about whichever
    of them happens to be indexed rather than about the directory the caller has in mind
    (`docs/adr/0007`).

    Args:
        location: A value with a scheme (`file://`, `s3://`) is returned as-is; a bare
            filesystem path becomes an absolute `file://` URI, with symlinks resolved to
            match how the ingester canonicalizes a root before recording it. On macOS `/tmp`
            is `/private/tmp`, so the two spellings are not interchangeable and the
            unresolved one matches no row at all.
    """
    text = os.fspath(location)
    if "://" in text:
        return text.rstrip("/")
    return "file://" + str(Path(text).resolve())


def to_upath(uri: str) -> UPath:
    """A single openable/globbable handle for `uri` (local or `s3://`)."""
    return UPath(uri)


def to_local_path(uri: str | UPath) -> Path:
    """The on-disk `pathlib.Path` for a `file://` URI.

    Args:
        uri: Accepts a `upath.UPath` as well as a string, because that is what
            `BidsLake.resolve` and `sibling_path` hand back and `str()` of one is the URI —
            so requiring the string turned every call site into
            `to_local_path(str(lake.resolve(...)))`.

    Raises:
        RemotePathError: For any non-local scheme.
    """
    text = str(uri)
    if not text.startswith("file://"):
        raise RemotePathError(
            f"{text!r} is not a local file:// URI; use `.path` (a UPath) or `.open()`"
        )
    return Path(text[len("file://") :])
