"""Resolving dataset-relative file paths to openable handles.

``root_uri`` (stored once per dataset on ``dataset_description``) is
``file:///abs/path`` for a locally-ingested dataset or ``s3://bucket/prefix`` for
an S3 one. The join itself is done in Rust (``_bidslake.resolve_uri``) so it
matches exactly how the ingester formats those URIs; this module wraps the
result in a :class:`upath.UPath` so callers get one handle that works for local
and remote alike.
"""

from __future__ import annotations

import os
from pathlib import Path

from upath import UPath


class RemotePathError(RuntimeError):
    """Raised when a local filesystem path is requested for a remote URI."""


def to_uri(location: str | os.PathLike[str]) -> str:
    """Normalize a filesystem path or URI to a URI (no trailing slash).

    A value with a scheme (``file://``, ``s3://``) is returned as-is; a bare
    filesystem path becomes an absolute ``file://`` URI. Used to rebase
    ``root_uri`` when a dataset has moved (``open(..., base_dir=/root_override=)``).
    """
    text = os.fspath(location)
    if "://" in text:
        return text.rstrip("/")
    return "file://" + str(Path(text).resolve())


def to_upath(uri: str) -> UPath:
    """A single openable/globbable handle for ``uri`` (local or ``s3://``)."""
    return UPath(uri)


def to_local_path(uri: str) -> Path:
    """The on-disk :class:`~pathlib.Path` for a ``file://`` URI.

    Raises :class:`RemotePathError` for any non-local scheme.
    """
    if not uri.startswith("file://"):
        raise RemotePathError(
            f"{uri!r} is not a local file:// URI; use `.path` (a UPath) or `.open()`"
        )
    return Path(uri[len("file://") :])
