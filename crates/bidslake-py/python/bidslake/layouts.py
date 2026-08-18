"""Layouts: naming an output file before it exists.

A query resolves what a unit *consumes* — files the catalog already knows about. A
layout is the other direction: where a unit's outputs *go*. Nothing can query for a file
a pipeline has not written yet, so without this every consumer hardcodes the convention,
which is how a script ends up with two dozen properties that are only string joins:

    @property
    def highres2standard_mat(self) -> Path:
        return self.outdir / "reg" / "highres2standard.mat"

With a layout that becomes a lookup, and the convention lives in one declared place:

    out = bidslake.layout("feat").under(dst / stem)
    out["highres2standard_mat"]     # <dst>/<stem>/reg/highres2standard.mat
    out["filtered_func_clean"]      # <dst>/<stem>/filtered_func_data_clean.nii.gz

The term map that reads such a tree back cannot simply be run backwards — it is
non-invertible, and ADR 0002 measures why. So a layout is its own artifact, and the two
are kept honest at load time: loading one renders every role under every declared example
and feeds each result back through the term map it names, raising unless
`classify(render(role))` reproduces the role's declared concepts.

Two consequences for a caller. A role whose placeholders nothing has bound raises rather
than rendering a plausible wrong path. And a role describes its file *at the destination*,
so its concepts are routinely narrower than a source-side query needs — a role is not a
catalog filter.
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

    Attributes:
        role: The role this answers for.
        exists: Whether the file is there.
        size_bytes: The file's size, or `None` when it is absent.
        mtime_ns: The file's modification time in nanoseconds, or `None` when it is absent.
    """

    role: str
    exists: bool
    size_bytes: int | None = None
    mtime_ns: int | None = None


@dataclasses.dataclass(frozen=True, slots=True)
class LayoutAt:
    """A layout bound to one unit's output root: role name in, path out.

    Attributes:
        layout: The layout whose roles this renders.
        root: The output root every rendered path is joined onto.
        bindings: Placeholder values every lookup through this binding inherits — see
            `Layout.under`. A per-call keyword to `path` overrides one.
    """

    layout: Layout
    root: Path
    bindings: Mapping[str, str] = dataclasses.field(default_factory=dict)

    def __getitem__(self, role: str) -> Path:
        """`path` with nothing bound per call, so a role reads as a lookup."""
        return self.path(role)

    def path(self, role: str, **bindings: str) -> Path:
        """The absolute path for `role` under this root.

        Args:
            role: A role name this layout declares.
            **bindings: Placeholder values for this call, merged over whatever
                `Layout.under` bound, and winning — so a run-wide value is stated once and
                a per-call one still overrides it.

        Raises:
            KeyError: An unknown role, an unbound `{placeholder}`, or a binding carrying a
                path separator — rather than returning a guess. The first is a typo, the
                second would otherwise render as a plausible path pointing at the wrong
                file, and the third one that leaves this root entirely.
        """
        merged = {**self.bindings, **bindings}
        rel = self.layout._inner.render(role, merged)
        if rel is None:
            known = ", ".join(self.layout.roles)
            if role not in self.layout.roles:
                msg = f"unknown role {role!r}; this layout declares: {known}"
            else:
                # Naming what *is* bound is what makes a mistyped binding visible. Without
                # it, `under(root, trainng=…)` fails for a caller who is certain they bound
                # it.
                #
                # Both causes are named rather than guessed at. `render` returns None for an
                # unbound placeholder *and* for bindings that would render a path leaving the
                # root, and which one applies depends on the role's template — which is on
                # the Rust side. Inferring it from the binding values gets it wrong in both
                # directions: `training='..'` is perfectly safe in a mid-segment placeholder
                # (it renders `fix4melview_.._thr20.txt`, one component), while `'x/../..'`
                # is not.
                have = ", ".join(f"{k}={v!r}" for k, v in sorted(merged.items())) or "nothing"
                msg = (
                    f"role {role!r} could not be rendered; bound here: {have}. Either a "
                    f"{{placeholder}} is unbound — pass it as a keyword, e.g. "
                    f"path({role!r}, training='UKBiobank'), or bind it once with "
                    f"layout.under(root, training='UKBiobank') — or a binding would put the "
                    f"path outside {self.root}, which is refused."
                )
            raise KeyError(msg)
        return self.root / rel

    def mkdir(self, role: str, **bindings: str) -> Path:
        """`path`, with the parent directory created. Returns the path."""
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

        Args:
            *roles: Restrict the answer to these roles. With none, every role this binding
                can *render* — a role whose placeholders nothing has bound is omitted rather
                than reported absent, because unaddressable and missing are different
                answers and conflating them reports a finished tree as incomplete for a
                forgotten keyword.
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
        deliberately differs from `path` — a caller asking "is it there" wants an
        answer, not a `KeyError`.
        """
        return any(st.exists for st in self.states(role))

    def present(self) -> dict[str, bool]:
        """`{role: exists}` for every renderable role. The shape a progress count wants."""
        return {st.role: st.exists for st in self.states()}

    def state(self, role: str) -> RoleState | None:
        """One role's `RoleState`, or `None` if it cannot be rendered."""
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

        Args:
            *roles: The roles to fold in. With none, every renderable role.
        """
        h = hashlib.sha256()
        for st in self.states(*roles):
            h.update(f"{st.role}:{st.exists}:{st.size_bytes}:{st.mtime_ns}\n".encode())
        return h.hexdigest()[:16]


@dataclasses.dataclass(frozen=True, slots=True)
class Layout:
    """A validated output layout, addressed by role name.

    Attributes:
        name: The bundled layout's name, as passed to `layout`.
    """

    name: str
    _inner: _bidslake.PyLayout

    @property
    def roles(self) -> tuple[str, ...]:
        """Every role this layout declares, sorted by name — the order `states` reports."""
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

        Most roles are fully determined by the root, so `under(root)` is the whole call.
        A few are not: `feat`'s two `classification` roles render
        `fix4melview_{training}_thr{threshold}.txt`, and those values are the pipeline's
        configuration — which FIX model, which threshold — not something the layout can
        know. Such a role raises rather than guessing (docs/adr/0002), which is right.

        Those values are constant for a whole run, so they are bound here rather than per
        access. Otherwise `for role in layout.roles: at[role]` is an error and every
        consumer that walks the role list special-cases the same two names by hand:

            out = layout("feat").under(dst / stem, training="UKBiobank", threshold="1")
            out["classification"]                                  # now just works
            out.path("classification_by_rater", rater="psadil")    # merged over the above

        Binding early moves the moment, not the guarantee: a placeholder *nothing* has
        bound still raises.

        Args:
            root: The unit's output root. Every path the binding renders is under it.
            **bindings: Placeholder values every lookup through the binding inherits. A
                per-call keyword to `LayoutAt.path` is merged over these and wins.
        """
        return LayoutAt(self, Path(root), dict(bindings))

    def __repr__(self) -> str:
        """The layout's name and how many roles it declares."""
        return f"Layout({self.name!r}, {len(self.roles)} roles)"


def layout(name: str) -> Layout:
    """Load a bundled layout by name (`feat`, `freesurfer`).

    Loading runs the round-trip check described in the module docstring, so a layout
    that has drifted from its term map raises here rather than at write time.

    An unknown name raises, and the message lists what is bundled — the two names above are
    every producer that ships a term map, which is the precondition a layout has.
    """
    return Layout(name=name, _inner=_bidslake.PyLayout(name))
