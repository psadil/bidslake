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
import os
from collections.abc import Mapping
from pathlib import Path

from . import _bidslake


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

        Raises rather than returning a guess: an unknown role is a typo, and an unbound
        ``{placeholder}`` would otherwise render as a plausible path pointing at the
        wrong file.

        Keywords here are merged over whatever :meth:`Layout.under` bound, and win — so a
        run-wide value is stated once and a per-call one still overrides it.
        """
        merged = {**self.bindings, **bindings}
        rel = self.layout._inner.render(role, merged)
        if rel is None:
            known = ", ".join(self.layout.roles)
            if role not in self.layout.roles:
                msg = f"unknown role {role!r}; this layout declares: {known}"
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
