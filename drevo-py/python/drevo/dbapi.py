"""PEP 249 (DB-API 2.0) interface to drevo, with Cypher as the query language (#555).

    import drevo.dbapi as dbapi
    conn = dbapi.connect("/path/graph.drevo")      # or ":memory:", or a drevo.Drevo
    cur = conn.cursor()
    cur.execute("MATCH (n:note) WHERE n.title = $t RETURN n.title", {"t": "x"})
    cur.fetchall(); cur.description
    conn.commit(); conn.rollback(); conn.close()

Parameters are named and use **Cypher's own `$name` syntax**: PEP 249's
`:name` form would collide with Cypher labels (`(n:Label)`), so queries are
passed through unchanged. Pass parameters as a mapping. `datetime.date`,
`datetime.time` and `datetime.datetime` values are sent as ISO-8601 strings,
the representation drevo's own `datetime()` returns.

Each connection runs an implicit transaction (DB-API semantics) on top of
`Drevo.begin()`. It starts with the first statement, `commit()` applies it
and `rollback()` discards it. Closing the connection, or leaving a `with`
block on an exception, rolls it back; leaving the `with` block cleanly
commits. A statement that fails rolls the transaction back, as drevo's
transactions do. Commit is optimistic: a concurrent write since the
transaction began makes `commit()` raise `OperationalError`, and the work
can be retried.

`threadsafety = 1`: threads may share the module but not connections.

Two-phase commit (#556) is available through PEP 249's optional TPC
extension: `conn.xid(format_id, gtrid, bqual)`, `tpc_begin(xid)`,
`tpc_prepare()`, `tpc_commit([xid])`, `tpc_rollback([xid])` and
`tpc_recover()`. A prepared branch is durable, survives its connection and a
restart, and can be resolved from any connection. While it is prepared,
every other write raises `OperationalError` (retryable).
"""

from __future__ import annotations

import datetime as _dt
import json as _json
import time as _time
from collections.abc import Iterator, Mapping, Sequence
from typing import Any, NamedTuple, Optional, Union

from . import (
    ConflictError,
    CypherError,
    Drevo,
    HeuristicRollbackError,
    DrevoError,
    LockedError,
    NotFoundError,
    PanicError,
    QueryTimeoutError,
    StorageError,
    Transaction,
    TransactionConflict,
    TransactionError,
)

__all__ = [
    "apilevel",
    "threadsafety",
    "paramstyle",
    "connect",
    "Connection",
    "Cursor",
    "Xid",
    "Warning",
    "Error",
    "InterfaceError",
    "DatabaseError",
    "DataError",
    "OperationalError",
    "IntegrityError",
    "InternalError",
    "ProgrammingError",
    "NotSupportedError",
    "Date",
    "Time",
    "Timestamp",
    "DateFromTicks",
    "TimeFromTicks",
    "TimestampFromTicks",
    "Binary",
    "STRING",
    "BINARY",
    "NUMBER",
    "DATETIME",
    "ROWID",
]

apilevel = "2.0"
threadsafety = 1
paramstyle = "named"

# ── Exceptions (PEP 249 hierarchy) ─────────────────────────────────────


class Warning(Exception):  # noqa: A001 — the name is mandated by PEP 249
    """Important warnings (PEP 249)."""


class Error(Exception):
    """Base class of every DB-API error."""


class InterfaceError(Error):
    """Misuse of the interface itself, e.g. a closed connection or cursor."""


class DatabaseError(Error):
    """Errors related to the database."""


class DataError(DatabaseError):
    """A value is out of range or cannot be represented."""


class OperationalError(DatabaseError):
    """Errors in the database's operation: a conflicting commit (retry), a
    statement timeout, storage or lock failures."""


class IntegrityError(DatabaseError):
    """Relational integrity is affected, e.g. a duplicate title."""


class InternalError(DatabaseError):
    """The database hit an internal error (a caught Rust panic)."""


class ProgrammingError(DatabaseError):
    """A malformed statement, a missing parameter, or a wrong call sequence."""


class NotSupportedError(DatabaseError):
    """A method or value the database does not support (e.g. binary data)."""


def _translate(exc: BaseException) -> Error:
    """The DB-API error for an exception raised by the drevo binding."""
    if isinstance(
        exc,
        (TransactionConflict, QueryTimeoutError, StorageError, LockedError, HeuristicRollbackError),
    ):
        return OperationalError(str(exc))
    if isinstance(exc, ConflictError):  # DuplicateTitleError, ConstraintViolation
        return IntegrityError(str(exc))
    if isinstance(exc, CypherError):
        return ProgrammingError(str(exc))
    if isinstance(exc, PanicError):
        return InternalError(str(exc))
    if isinstance(exc, TransactionError):
        return ProgrammingError(str(exc))
    if isinstance(exc, NotFoundError):
        return DatabaseError(str(exc))
    if isinstance(exc, DrevoError):
        return DatabaseError(str(exc))
    if isinstance(exc, (OverflowError, ValueError)):
        return DataError(str(exc))
    if isinstance(exc, TypeError):
        return ProgrammingError(str(exc))
    if isinstance(exc, RuntimeError):
        return InterfaceError(str(exc))
    return DatabaseError(str(exc))


# ── Type objects and constructors ──────────────────────────────────────


class _TypeObject:
    """Compares equal to every Python type of one DB-API category."""

    def __init__(self, *types: type) -> None:
        self._types = types

    def __eq__(self, other: object) -> bool:
        return other in self._types

    def __ne__(self, other: object) -> bool:
        return not self.__eq__(other)

    def __hash__(self) -> int:
        return hash(self._types)

    def __repr__(self) -> str:
        return f"_TypeObject({', '.join(t.__name__ for t in self._types)})"


STRING = _TypeObject(str)
BINARY = _TypeObject(bytes, bytearray)
NUMBER = _TypeObject(int, float, bool)
DATETIME = _TypeObject(_dt.datetime, _dt.date, _dt.time)
ROWID = _TypeObject(int)

Date = _dt.date
Time = _dt.time
Timestamp = _dt.datetime


def DateFromTicks(ticks: float) -> _dt.date:  # noqa: N802 — PEP 249 name
    """A date from seconds since the epoch (local time)."""
    return Date(*_time.localtime(ticks)[:3])


def TimeFromTicks(ticks: float) -> _dt.time:  # noqa: N802 — PEP 249 name
    """A time from seconds since the epoch (local time)."""
    return Time(*_time.localtime(ticks)[3:6])


def TimestampFromTicks(ticks: float) -> _dt.datetime:  # noqa: N802 — PEP 249 name
    """A timestamp from seconds since the epoch (local time)."""
    return Timestamp(*_time.localtime(ticks)[:6])


def Binary(value: Union[bytes, bytearray, memoryview]) -> bytes:  # noqa: N802 — PEP 249 name
    """A binary value. drevo has no byte strings, so passing one as a
    parameter raises `NotSupportedError`."""
    return bytes(value)


def _prepare(value: Any) -> Any:
    """Convert DB-API parameter values the binding does not take natively."""
    if isinstance(value, (_dt.datetime, _dt.date, _dt.time)):
        return value.isoformat()
    if isinstance(value, (bytes, bytearray, memoryview)):
        raise NotSupportedError("binary parameters are not supported (Cypher has no byte strings)")
    if isinstance(value, Mapping):
        return {k: _prepare(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_prepare(v) for v in value]
    return value


def _type_code(values: Sequence[Any]) -> Optional[type]:
    """The Python type of the first non-null value in a column, if any."""
    for value in values:
        if value is not None:
            return type(value)
    return None


# ── Two-phase commit (PEP 249 TPC extension) ──────────────────────────


class Xid(NamedTuple):
    """A PEP 249 transaction id: `(format_id, global_transaction_id,
    branch_qualifier)`."""

    format_id: int
    gtrid: str
    bqual: str


_XID_PREFIX = "dbapi-xid:"


def _xid_to_gid(xid: Xid) -> str:
    return _XID_PREFIX + _json.dumps([xid.format_id, xid.gtrid, xid.bqual])


def _gid_to_xid(gid: str) -> Optional[Xid]:
    """The `Xid` a gid encodes, or `None` for a gid not made by this module."""
    if not gid.startswith(_XID_PREFIX):
        return None
    try:
        format_id, gtrid, bqual = _json.loads(gid[len(_XID_PREFIX) :])
        return Xid(int(format_id), str(gtrid), str(bqual))
    except (ValueError, TypeError):
        return None


# ── Connection ─────────────────────────────────────────────────────────


def connect(database: Union[str, "Drevo"] = ":memory:") -> "Connection":
    """Open a connection. `database` is a path to a drevo store (created if
    missing), `":memory:"` for an ephemeral one, or an open `drevo.Drevo`
    handle, which the connection uses but does not close."""
    try:
        if isinstance(database, Drevo):
            return Connection(database, owns_handle=False)
        if database == ":memory:":
            return Connection(Drevo.open_in_memory(), owns_handle=True)
        return Connection(Drevo.open(database), owns_handle=True)
    except Error:
        raise
    except Exception as exc:  # noqa: BLE001 — mapped to the DB-API hierarchy
        raise _translate(exc) from exc


class Connection:
    """A DB-API connection with an implicit transaction."""

    Warning = Warning
    Error = Error
    InterfaceError = InterfaceError
    DatabaseError = DatabaseError
    DataError = DataError
    OperationalError = OperationalError
    IntegrityError = IntegrityError
    InternalError = InternalError
    ProgrammingError = ProgrammingError
    NotSupportedError = NotSupportedError

    def __init__(self, handle: Drevo, owns_handle: bool) -> None:
        self._handle: Optional[Drevo] = handle
        self._owns_handle = owns_handle
        self._tx: Optional[Transaction] = None
        # PEP 249 TPC state: the xid of the current two-phase transaction and
        # whether it has been prepared.
        self._tpc_xid: Optional[Xid] = None
        self._tpc_prepared = False

    def _check_open(self) -> Drevo:
        if self._handle is None:
            raise InterfaceError("connection is closed")
        return self._handle

    def _transaction(self) -> Transaction:
        """The implicit transaction, begun on first use."""
        handle = self._check_open()
        if self._tx is None or self._tx.closed:
            try:
                self._tx = handle.begin()
            except Exception as exc:  # noqa: BLE001
                raise _translate(exc) from exc
        return self._tx

    def cursor(self) -> "Cursor":
        """A new cursor on this connection."""
        self._check_open()
        return Cursor(self)

    def commit(self) -> None:
        """Apply the implicit transaction (a no-op if nothing ran)."""
        self._check_open()
        if self._tpc_xid is not None:
            raise ProgrammingError(
                "commit() is not allowed inside a TPC transaction; use tpc_commit()"
            )
        tx, self._tx = self._tx, None
        if tx is None or tx.closed:
            return
        try:
            tx.commit()
        except Exception as exc:  # noqa: BLE001
            raise _translate(exc) from exc

    def rollback(self) -> None:
        """Discard the implicit transaction (a no-op if nothing ran)."""
        self._check_open()
        if self._tpc_xid is not None:
            raise ProgrammingError(
                "rollback() is not allowed inside a TPC transaction; use tpc_rollback()"
            )
        tx, self._tx = self._tx, None
        if tx is not None and not tx.closed:
            tx.rollback()

    def close(self) -> None:
        """Roll back pending work and release the database (idempotent). A
        *prepared* TPC branch is left prepared, for the coordinator."""
        if self._handle is None:
            return
        self._tpc_xid = None
        self._tpc_prepared = False
        try:
            self.rollback()
        finally:
            handle, self._handle = self._handle, None
            if self._owns_handle:
                handle.close()

    # ── PEP 249 two-phase-commit extension ────────────────────────────

    def xid(self, format_id: int, gtrid: str, bqual: str) -> Xid:
        """A transaction id for `tpc_begin`."""
        return Xid(format_id, gtrid, bqual)

    def tpc_begin(self, xid: Xid) -> None:
        """Start a two-phase transaction with id `xid`."""
        self._check_open()
        if self._tpc_xid is not None or (self._tx is not None and not self._tx.closed):
            raise ProgrammingError("a transaction is already in progress")
        self._tpc_xid = Xid(*xid)
        self._tpc_prepared = False

    def tpc_prepare(self) -> None:
        """Phase one: durably prepare the current two-phase transaction."""
        self._check_open()
        if self._tpc_xid is None or self._tpc_prepared:
            raise ProgrammingError("tpc_prepare() needs an unprepared tpc_begin() transaction")
        tx = self._transaction()
        self._tx = None
        try:
            tx.prepare(_xid_to_gid(self._tpc_xid))
        except Exception as exc:  # noqa: BLE001
            self._tpc_xid = None
            raise _translate(exc) from exc
        self._tpc_prepared = True

    def _resolve(self, xid: Optional[Xid], commit: bool) -> None:
        handle = self._check_open()
        if xid is not None:
            # Recovery: resolve a prepared branch by id, outside any transaction.
            if self._tpc_xid is not None:
                raise ProgrammingError("resolve a recovered xid outside a TPC transaction")
            gid = _xid_to_gid(Xid(*xid))
        else:
            if self._tpc_xid is None:
                raise ProgrammingError("no TPC transaction in progress")
            current, prepared = self._tpc_xid, self._tpc_prepared
            self._tpc_xid, self._tpc_prepared = None, False
            if not prepared:
                # One-phase: finish the implicit transaction directly.
                tx, self._tx = self._tx, None
                if tx is None or tx.closed:
                    return
                try:
                    if commit:
                        tx.commit()
                    else:
                        tx.rollback()
                except Exception as exc:  # noqa: BLE001
                    raise _translate(exc) from exc
                return
            gid = _xid_to_gid(current)
        try:
            if commit:
                handle.commit_prepared(gid)
            else:
                handle.rollback_prepared(gid)
        except Exception as exc:  # noqa: BLE001
            raise _translate(exc) from exc

    def tpc_commit(self, xid: Optional[Xid] = None) -> None:
        """Phase two: commit the current two-phase transaction (one-phase if it
        was never prepared), or, given `xid`, a recovered prepared branch."""
        self._resolve(xid, commit=True)

    def tpc_rollback(self, xid: Optional[Xid] = None) -> None:
        """Roll back the current two-phase transaction, or, given `xid`, a
        recovered prepared branch."""
        self._resolve(xid, commit=False)

    def tpc_recover(self) -> list[Xid]:
        """Every prepared branch created through this module, pending resolution."""
        handle = self._check_open()
        try:
            gids = [p.gid for p in handle.list_prepared()]
        except Exception as exc:  # noqa: BLE001
            raise _translate(exc) from exc
        return [xid for gid in gids if (xid := _gid_to_xid(gid)) is not None]

    def __enter__(self) -> "Connection":
        self._check_open()
        return self

    def __exit__(self, exc_type: Any, exc_value: Any, traceback: Any) -> None:
        """Commit on a clean exit, roll back on an exception; then close."""
        try:
            if exc_type is None:
                self.commit()
            else:
                self.rollback()
        finally:
            self.close()


# ── Cursor ─────────────────────────────────────────────────────────────

_WRITE_COUNTERS = (
    "nodes_created",
    "nodes_deleted",
    "relationships_created",
    "relationships_deleted",
)
_UPDATE_COUNTERS = ("properties_set", "labels_added", "labels_removed")


class Cursor:
    """A DB-API cursor; rows are tuples in `RETURN` column order."""

    def __init__(self, connection: Connection) -> None:
        self._connection: Optional[Connection] = connection
        self.arraysize = 1
        self.description: Optional[list[tuple[Any, ...]]] = None
        self.rowcount = -1
        self.lastrowid: Optional[int] = None
        self._rows: Optional[list[tuple[Any, ...]]] = None
        self._pos = 0

    @property
    def connection(self) -> Connection:
        """The connection this cursor runs on."""
        if self._connection is None:
            raise InterfaceError("cursor is closed")
        return self._connection

    def close(self) -> None:
        """Close the cursor; later use raises `InterfaceError`."""
        self._connection = None
        self._rows = None

    def _run(self, operation: str, parameters: Optional[Mapping[str, Any]]) -> int:
        """Execute one statement in the implicit transaction; return its rowcount."""
        connection = self.connection
        if parameters is not None and not isinstance(parameters, Mapping):
            raise ProgrammingError("parameters must be a mapping of $name -> value")
        params = _prepare(dict(parameters)) if parameters is not None else None
        tx = connection._transaction()
        try:
            result = tx.execute(operation, params)
        except Exception as exc:  # noqa: BLE001
            raise _translate(exc) from exc
        columns = result.columns
        rows = [tuple(row[c] for c in columns) for row in result.rows]
        if columns:
            self.description = [
                (name, _type_code([r[i] for r in rows]), None, None, None, None, None)
                for i, name in enumerate(columns)
            ]
            self._rows = rows
            self._pos = 0
            return len(rows)
        self.description = None
        self._rows = []
        self._pos = 0
        stats = result.stats
        affected = sum(stats[k] for k in _WRITE_COUNTERS)
        return affected if affected else sum(stats[k] for k in _UPDATE_COUNTERS)

    def execute(self, operation: str, parameters: Optional[Mapping[str, Any]] = None) -> "Cursor":
        """Run one Cypher statement. `rowcount` is the number of rows returned,
        or for a write without `RETURN` the number of nodes and relationships
        created or deleted (else properties and labels changed)."""
        self.rowcount = self._run(operation, parameters)
        return self

    def executemany(
        self, operation: str, seq_of_parameters: Sequence[Mapping[str, Any]]
    ) -> "Cursor":
        """Run the statement once per parameter mapping; `rowcount` is the total."""
        total = 0
        for parameters in seq_of_parameters:
            total += self._run(operation, parameters)
        self.rowcount = total
        return self

    def _result(self) -> list[tuple[Any, ...]]:
        self.connection  # raises InterfaceError when closed
        if self._rows is None:
            raise ProgrammingError("no statement has been executed")
        return self._rows

    def fetchone(self) -> Optional[tuple[Any, ...]]:
        """The next row, or `None` when exhausted."""
        rows = self._result()
        if self._pos >= len(rows):
            return None
        row = rows[self._pos]
        self._pos += 1
        return row

    def fetchmany(self, size: Optional[int] = None) -> list[tuple[Any, ...]]:
        """Up to `size` (default `arraysize`) more rows."""
        rows = self._result()
        n = self.arraysize if size is None else size
        chunk = rows[self._pos : self._pos + n]
        self._pos += len(chunk)
        return chunk

    def fetchall(self) -> list[tuple[Any, ...]]:
        """Every remaining row."""
        rows = self._result()
        chunk = rows[self._pos :]
        self._pos = len(rows)
        return chunk

    def setinputsizes(self, sizes: Any) -> None:
        """Accepted and ignored (PEP 249 allows it to do nothing)."""

    def setoutputsize(self, size: Any, column: Optional[int] = None) -> None:
        """Accepted and ignored (PEP 249 allows it to do nothing)."""

    def __iter__(self) -> Iterator[tuple[Any, ...]]:
        while (row := self.fetchone()) is not None:
            yield row
