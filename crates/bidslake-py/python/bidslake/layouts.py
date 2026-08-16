"""Layouts: naming an output file before it exists.

A query resolves what a unit *consumes* — files the catalog already knows about. A
layout is the other direction: where a unit's outputs *go*. Nothing can query for a file
a pipeline has not written yet, so without this every consumer hardcodes the convention,
which is how a script ends up with two dozen properties that are only string joins::

    @property
    def highres2standard_mat(self) -> Path:
        return self.outdir / "reg" / "highres2standard.mat"

With a layout that becomes a lookup, and the convention lives in one declared place::

    out = bidslake.layout("feat").under(dst / stem)
    out["highres2standard_mat"]     # <dst>/<stem>/reg/highres2standard.mat
    out["filtered_func_clean"]      # <dst>/<stem>/filtered_func_data_clean.nii.gz

Why this is a separate artifact from the term map
-------------------------------------------------
The term map already parses these exact paths, so the obvious question is why it cannot
simply be run backwards. Its templates are PCRE, pinned because optional groups collapse
FreeSurfer's ``sub-01_ses-1`` / ``sub-01`` / bare ``bert`` forms into one mapping — and
that collapsing is precisely what makes them non-invertible.

Swapping PCRE for ``{var}`` does not rescue it, because invertibility is a property of
the *mapping*, not of the syntax: a mapping that recognizes a whole class of filenames
(``mri/*.mgz``, ``label/*.annot``) has no concept to render *from*. Measured on a real
recon-all tree, a pure-``{var}`` rewrite needed 50% more mappings and still lost a third
of the files. So the read direction keeps PCRE, and the roles that *can* be written are
declared separately.

What stops the two drifting
---------------------------
Every layout declares ``Examples``, and loading one renders **every role under every
example** and feeds the result back through its term map. If ``classify(render(role))``
does not reproduce the role's declared concepts, the layout raises rather than loading.
The two directions are therefore checked against each other, not merely kept side by
side — co-locating them would only have prevented *textual* drift.

That check earns its keep immediately: it caught a role (``reg/wmparc.nii.gz``) that the
pipeline writes but the term map had no mapping for, so those files were being produced
and then silently ignored at index time.
"""

from __future__ import annotations

import dataclasses
import hashlib
import os
from collections.abc import Mapping
from pathlib import Path

from . import _bidslake


@dataclasses.dataclass(frozen=True, slots=True)
class RoleState:
    """What the filesystem says about one role's file.

    Size and mtime rather than a checksum, and the distinction is the point: a digest would
    mean reading every file, while these two answer "has this changed since I looked" for
    the price of a `stat`. That is the question a workflow engine's staleness check asks,
    and the one `bidslake verify` asks of a catalog — the same pair of numbers the ingest
    records in `all_files.size_bytes`/`mtime_ns`, so the two are directly comparable.
    """

    role: str
    exists: bool
    size_bytes: int | None = None
    mtime_ns: int | None = None


@dataclasses.dataclass(frozen=True, slots=True)
class LayoutAt:
    """A layout bound to one unit's output root: role name in, path out."""

    layout: Layout
    root: Path
    #: Placeholder values every lookup through this binding inherits — see
    #: :meth:`Layout.under`. A per-call keyword to :meth:`path` overrides one.
    bindings: Mapping[str, str] = dataclasses.field(default_factory=dict)

    def __getitem__(self, role: str) -> Path:
        return self.path(role)

    def path(self, role: str, **bindings: str) -> Path:
        """The absolute path for ``role`` under this root.

        Raises rather than returning a guess: an unknown role is a typo, an unbound
        ``{placeholder}`` would otherwise render as a plausible path pointing at the
        wrong file, and a binding carrying a path separator would render one that leaves
        this root entirely.

        Keywords here are merged over whatever :meth:`Layout.under` bound, and win — so a
        run-wide value is stated once and a per-call one still overrides it.
        """
        merged = {**self.bindings, **bindings}
        rel = self.layout._inner.render(role, merged)
        if rel is None:
            known = ", ".join(self.layout.roles)
            # A binding is a label, not a path fragment. One carrying a separator or a
            # parent reference renders a path that normalizes outside this root — and
            # `mkdir` below would then create directories there — so `render` refuses it.
            # Checked before the unbound-placeholder branch because both surface as `None`,
            # and reporting an unsafe binding as a missing one sends the caller looking for
            # a keyword they already passed.
            unsafe = {k: v for k, v in sorted(merged.items()) if "/" in v or v == ".."}
            if role not in self.layout.roles:
                msg = f"unknown role {role!r}; this layout declares: {known}"
            elif unsafe:
                offending = ", ".join(f"{k}={v!r}" for k, v in unsafe.items())
                msg = (
                    f"role {role!r} cannot be rendered: {offending} would put the path "
                    f"outside {self.root}. A binding names one path component, so it may "
                    f"not contain '/' or '..'."
                )
            else:
                # Naming what *is* bound is what makes a mistyped binding visible. Without
                # it, `under(root, trainng=…)` fails with "unbound placeholders" for a
                # caller who is certain they bound it.
                have = ", ".join(f"{k}={v!r}" for k, v in sorted(merged.items())) or "nothing"
                msg = (
                    f"role {role!r} has unbound placeholders; bound here: {have}. "
                    f"Pass them as keywords, e.g. path({role!r}, training='UKBiobank'), "
                    f"or bind them once with layout.under(root, training='UKBiobank')"
                )
            raise KeyError(msg)
        return self.root / rel

    def mkdir(self, role: str, **bindings: str) -> Path:
        """:meth:`path`, with the parent directory created. Returns the path."""
        target = self.path(role, **bindings)
        target.parent.mkdir(parents=True, exist_ok=True)
        return target

    # -- what is actually here -------------------------------------------------------
    #
    # A layout says where a role *goes*; these say whether it arrived. Every consumer that
    # tracks progress through a tree needs that, and before this each wrote its own: a
    # pipeline's own "is this step done" check, a workflow engine's completion rule, an
    # asset check comparing a ledger against disk, and a progress count for a UI. Four
    # implementations of one question, each stat-ing the same paths.
    #
    # The work is in Rust (`PyLayout::present`), so the answer here and the one
    # `bidslake verify` gives for a catalog come from one place and one meaning of
    # "changed".

    def states(self, *roles: str) -> list[RoleState]:
        """Every role's presence and stat, in the layout's order.

        With no arguments, every role this binding can *render* — a role whose placeholders
        nothing has bound is omitted rather than reported absent, because unaddressable and
        missing are different answers and conflating them reports a finished tree as
        incomplete for a forgotten keyword.
        """
        rows = self.layout._inner.present(str(self.root), dict(self.bindings))
        states = [RoleState(r, e, sz, mt) for (r, e, sz, mt) in rows]
        if roles:
            wanted = set(roles)
            states = [st for st in states if st.role in wanted]
        return states

    def has(self, role: str) -> bool:
        """Is this one role's file there?

        `False` for a role that cannot be rendered, which is the one place this
        deliberately differs from :meth:`path` — a caller asking "is it there" wants an
        answer, not a `KeyError`.
        """
        return any(st.exists for st in self.states(role))

    def present(self) -> dict[str, bool]:
        """`{role: exists}` for every renderable role. The shape a progress count wants."""
        return {st.role: st.exists for st in self.states()}

    def state(self, role: str) -> RoleState | None:
        """One role's :class:`RoleState`, or `None` if it cannot be rendered."""
        return next(iter(self.states(role)), None)

    def digest(self, *roles: str) -> str:
        """A short hash over the given roles' size and mtime — a cache key.

        This is what a content-addressed engine needs and what none of them can derive on
        their own: Dagster's `DataVersion`, a Prefect `cache_key_fn`, or anything else that
        must decide whether work already done is still valid. Absent roles are folded in as
        such, so a digest changes when a file appears or disappears as well as when one is
        rewritten.

        Not a content hash. Two files with identical size and mtime are treated as the same
        file, which is the same assumption `make` has always made and is wrong only for a
        deliberate forgery or a filesystem with second-granularity timestamps that was
        written to twice within one tick.
        """
        h = hashlib.sha256()
        for st in self.states(*roles):
            h.update(f"{st.role}:{st.exists}:{st.size_bytes}:{st.mtime_ns}\n".encode())
        return h.hexdigest()[:16]


@dataclasses.dataclass(frozen=True, slots=True)
class Layout:
    """A validated output layout, addressed by role name."""

    name: str
    _inner: _bidslake.PyLayout

    @property
    def roles(self) -> tuple[str, ...]:
        return tuple(self._inner.roles())

    @property
    def term_map(self) -> str:
        """The term map whose read direction this layout is checked against."""
        return self._inner.term_map()

    def describe(self, role: str) -> str | None:
        """What a role is, for a human reading the layout rather than the tree."""
        return self._inner.description(role)

    def under(self, root: str | os.PathLike[str], **bindings: str) -> LayoutAt:
        """Bind this layout to one unit's output root, and optionally its placeholders.

        Most roles are fully determined by the root, so ``under(root)`` is the whole call.
        A few are not: `feat`'s two `classification` roles render
        ``fix4melview_{training}_thr{threshold}.txt``, and those values are the pipeline's
        configuration — which FIX model, which threshold — not something the layout can
        know. Such a role raises rather than guessing (docs/adr/0008), which is right.

        What was wrong is *when* the values had to be supplied. They are constant for a
        whole run, and requiring them per access made
        ``for role in layout.roles: at[role]`` an error, so every consumer that walked the
        role list special-cased the same two names by hand::

            out = layout("feat").under(dst / stem, training="UKBiobank", threshold="1")
            out["classification"]                                  # now just works
            out.path("classification_by_rater", rater="psadil")    # merged over the above

        The guarantee is unchanged: a placeholder *nothing* has bound still raises. Only
        the moment of binding moved.
        """
        return LayoutAt(self, Path(root), dict(bindings))

    def __repr__(self) -> str:
        return f"Layout({self.name!r}, {len(self.roles)} roles)"


def layout(name: str) -> Layout:
    """Load a bundled layout by name (``feat``).

    Loading runs the round-trip check described in the module docstring, so a layout
    that has drifted from its term map raises here rather than at write time.
    """
    return Layout(name=name, _inner=_bidslake.PyLayout(name))
