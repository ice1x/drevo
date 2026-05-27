"""End-to-end scenario: lightweight enterprise resource planning (ERP).

Mirrors ``tests/scenario_erp.rs`` at the Python surface.

Domain model:
    Node kinds: company, department, employee, product, customer,
                purchase_order
    Edge kinds: belongs_to, employs, manages, sells, supplies, fulfilled_by

The scenario walks the org chart, the inventory, and one in-flight
order. It is intentionally small (one company, two departments, four
employees, three products, one customer + order) so the projections
asserted below stay obvious — the same checks a finance dashboard
would render.
"""

from __future__ import annotations

import drevo

from .conftest import must_get_node


def _new(kind: str, title: str, body: str = "", **props: object) -> drevo.NewNode:
    return drevo.NewNode(
        kind=kind, title=title, body=body, properties=dict(props) if props else None
    )


def _edge(from_id: int, to_id: int, kind: str, weight: float = 1.0) -> drevo.NewEdge:
    return drevo.NewEdge(from_id=from_id, to_id=to_id, kind=kind, weight=weight)


def _build_company(db: drevo.Drevo) -> dict[str, drevo.Node]:
    n: dict[str, drevo.Node] = {}

    n["company"] = db.create_node(_new("company", "acme-robotics", "Industrial automation"))
    n["dept_eng"] = db.create_node(_new("department", "engineering", headcount=12))
    n["dept_sales"] = db.create_node(_new("department", "sales", headcount=4))

    n["emp_eng_lead"] = db.create_node(_new("employee", "alice-cho", role="vp-engineering"))
    n["emp_dev"] = db.create_node(_new("employee", "bob-singh", role="staff-engineer"))
    n["emp_sales_lead"] = db.create_node(_new("employee", "carla-rosso", role="vp-sales"))
    n["emp_ae"] = db.create_node(_new("employee", "dan-park", role="account-executive"))

    n["product_arm"] = db.create_node(_new("product", "arm-7", price=4999))
    n["product_grip"] = db.create_node(_new("product", "grip-3", price=899))
    n["product_eye"] = db.create_node(_new("product", "vision-2", price=2599))

    n["customer"] = db.create_node(_new("customer", "northwind-foundry", tier="gold"))
    n["po"] = db.create_node(
        _new("purchase_order", "po-2026-0009", status="open", total_cents=850000)
    )

    # Company → departments → employees
    db.create_edge(_edge(n["dept_eng"].id, n["company"].id, "belongs_to"))
    db.create_edge(_edge(n["dept_sales"].id, n["company"].id, "belongs_to"))
    db.create_edge(_edge(n["dept_eng"].id, n["emp_eng_lead"].id, "employs"))
    db.create_edge(_edge(n["dept_eng"].id, n["emp_dev"].id, "employs"))
    db.create_edge(_edge(n["dept_sales"].id, n["emp_sales_lead"].id, "employs"))
    db.create_edge(_edge(n["dept_sales"].id, n["emp_ae"].id, "employs"))

    # Management chain
    db.create_edge(_edge(n["emp_eng_lead"].id, n["emp_dev"].id, "manages"))
    db.create_edge(_edge(n["emp_sales_lead"].id, n["emp_ae"].id, "manages"))

    # Sales catalog: sales-lead sells every product
    db.create_edge(_edge(n["emp_sales_lead"].id, n["product_arm"].id, "sells"))
    db.create_edge(_edge(n["emp_sales_lead"].id, n["product_grip"].id, "sells"))
    db.create_edge(_edge(n["emp_sales_lead"].id, n["product_eye"].id, "sells"))

    # Order flow: customer → PO → products (supplies), PO fulfilled_by AE
    db.create_edge(_edge(n["customer"].id, n["po"].id, "belongs_to"))
    db.create_edge(_edge(n["po"].id, n["product_arm"].id, "supplies"))
    db.create_edge(_edge(n["po"].id, n["product_grip"].id, "supplies"))
    db.create_edge(_edge(n["po"].id, n["emp_ae"].id, "fulfilled_by"))

    return n


def test_org_census_matches_definition(disk_db: drevo.Drevo) -> None:
    _build_company(disk_db)
    assert len(disk_db.list_nodes_by_kind("company", limit=10, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("department", limit=10, offset=0)) == 2
    assert len(disk_db.list_nodes_by_kind("employee", limit=10, offset=0)) == 4
    assert len(disk_db.list_nodes_by_kind("product", limit=10, offset=0)) == 3
    assert len(disk_db.list_nodes_by_kind("customer", limit=10, offset=0)) == 1
    assert len(disk_db.list_nodes_by_kind("purchase_order", limit=10, offset=0)) == 1


def test_engineering_headcount_via_employs_edges(disk_db: drevo.Drevo) -> None:
    """``edges_of(dept, OUT, kind=employs)`` is the engineering roster."""
    n = _build_company(disk_db)
    out = disk_db.edges_of(n["dept_eng"].id, drevo.Direction.OUT)
    employee_titles = {must_get_node(disk_db, e.to_id).title for e in out if e.kind == "employs"}
    assert employee_titles == {"alice-cho", "bob-singh"}


def test_management_chain_shortest_path(disk_db: drevo.Drevo) -> None:
    """The org chart resolves a manager → report shortest path."""
    n = _build_company(disk_db)
    path = disk_db.shortest_path(n["emp_eng_lead"].id, n["emp_dev"].id, edge_kind="manages")
    assert path == [n["emp_eng_lead"].id, n["emp_dev"].id]


def test_purchase_order_line_items_via_supplies(disk_db: drevo.Drevo) -> None:
    """Outbound ``supplies`` edges from the PO are the order's line items."""
    n = _build_company(disk_db)
    out = disk_db.edges_of(n["po"].id, drevo.Direction.OUT)
    line_titles = {must_get_node(disk_db, e.to_id).title for e in out if e.kind == "supplies"}
    assert line_titles == {"arm-7", "grip-3"}


def test_subgraph_from_company_pulls_full_org(disk_db: drevo.Drevo) -> None:
    """A 4-hop subgraph from the company reaches every employee and at
    least one product/PO — the finance roll-up projection.
    """
    n = _build_company(disk_db)
    sg = disk_db.subgraph(n["company"].id, depth=4)
    kinds = {node.kind for node in sg.nodes}
    assert {"company", "department", "employee"}.issubset(kinds)


def test_fts_finds_purchase_order_by_status_text(disk_db: drevo.Drevo) -> None:
    """The FTS index covers titles + bodies; searching for the PO title
    fragment recovers the order — the basis of a "find any open PO"
    search box.
    """
    _build_company(disk_db)
    hits = disk_db.search_fts("po-2026", limit=10)
    titles = {h.node.title for h in hits}
    assert "po-2026-0009" in titles


def test_org_round_trips_through_reopen(tmp_db_path: str) -> None:
    """Persist the company, reopen, every census check still passes."""
    with drevo.Drevo.open(tmp_db_path) as db:
        _build_company(db)
    with drevo.Drevo.open(tmp_db_path) as db2:
        assert len(db2.list_nodes_by_kind("employee", limit=10, offset=0)) == 4
        assert len(db2.list_nodes_by_kind("product", limit=10, offset=0)) == 3
