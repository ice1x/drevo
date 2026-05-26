"""Pure-Python exception subclasses for the drevo package.

The PyO3 layer in ``drevo-py/src/errors.rs`` already declares the full
typed exception hierarchy rooted at ``drevo.DrevoError``. This module
adds the single subclass that the cdylib **cannot** declare on its own
without dragging the pure-Python layer into a circular import:
:class:`InvalidWeightError`.

RFC §5.3 + §12.3 amendment: ``DrevoError::InvalidWeight`` maps to
``PyValueError`` in the Rust mapper (``drevo-py/src/errors.rs::map_err``)
because "you passed me a bad number" is the canonical Python idiom for
``ValueError``. Callers that want to narrow the catch to "specifically
the drevo invalid-weight case" need a typed subclass — but declaring
``class InvalidWeightError(ValueError)`` in Rust via ``create_exception!``
would conflict with the requirement that ``ValueError`` stay the parent
class (PyO3 ``create_exception!`` insists on a single parent path that
ultimately points at ``PyException``, not ``PyValueError``).

The compromise: the cdylib raises ``PyValueError`` directly with a
substring-matchable message; this pure-Python subclass exists so user
code can write ``except drevo.InvalidWeightError`` once the message
discipline is in place. The Python layer never raises this exception
itself (the cdylib does, as a plain ``ValueError``) — it is purely a
*type marker* that lets static analyzers and exception filters refer
to a stable name.

A future patch (tracked under the Phase 16 follow-ups) may wrap the
cdylib's ``PyValueError`` in this subclass at the Python boundary,
which would let ``except InvalidWeightError`` actually fire. For now
catching ``ValueError`` is the working pattern; catching
``InvalidWeightError`` succeeds for any *future* re-raise that uses
this class.
"""

from __future__ import annotations


class InvalidWeightError(ValueError):
    """Raised when an edge weight is non-finite (``NaN`` / ±``inf``).

    Subclass of :class:`ValueError` so that callers can rely on
    ``except ValueError:`` continuing to work — the Rust mapper
    currently raises a plain ``PyValueError`` with a substring-matchable
    message ("invalid edge weight: ..."). Once the Python boundary
    re-raises through this subclass (follow-up patch), ``except
    drevo.InvalidWeightError:`` will narrow the catch to drevo-specific
    failures without breaking the legacy pattern.
    """


__all__ = ["InvalidWeightError"]
