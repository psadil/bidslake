"""Decode the Arrow IPC bytes the native extension returns into Polars.

The Rust side serializes each result set to an Arrow IPC *stream*; Polars reads
it back with :func:`polars.read_ipc_stream`. This is the one place that bridges
the native query primitive to a DataFrame.
"""

from __future__ import annotations

import io

import polars as pl


def ipc_to_df(data: bytes) -> pl.DataFrame:
    """Read Arrow IPC stream ``data`` (from ``PyLake.query_ipc``) into Polars."""
    return pl.read_ipc_stream(io.BytesIO(data))
