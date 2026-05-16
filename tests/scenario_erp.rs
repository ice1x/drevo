//! Scenario integration test: ERP System
//!
//! This test validates drevo against a real-world ERP use case.
//!
//! Domain model:
//! - Node kinds: `order`, `product`, `customer`, `warehouse`, `invoice`
//! - Edge kinds: `ordered_by`, `contains`, `stored_in`, `billed_to`
//!
//! Tested workflows:
//! - Building an order book with customers, products, warehouses, orders, invoices
//! - Customer → Orders: finding all orders for a customer
//! - Order → Products: extracting the line items of an order
//! - Product → Warehouses: inventory distribution across warehouses
//! - Invoice → Order → Customer: billing chains
//! - Kind index: board-style views (all orders, all products, ...)
//! - FTS: searching products by name or description
//! - Subgraph: full context of an order (customer, line items, warehouses, invoice)
//! - Shortest path: customer → warehouse via order and product
//! - Edge properties: quantity on line items, stock level on warehouse edges
//! - Transactional updates: order status transitions, inventory changes
//! - Cascade delete: removing a product cleans up all contains/stored_in edges

use drevo::db::Drevo;
use drevo::model::*;
use std::collections::HashMap;

// =========================================================================
// Test helpers
// =========================================================================

fn memory_db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory DB")
}

#[cfg(feature = "redb-backend")]
fn redb_db(dir: &tempfile::TempDir) -> Drevo {
    let path = dir.path().join("erp_test.db");
    Drevo::open(&path).expect("open redb DB")
}

fn make_node_with_props(
    kind: &str,
    title: &str,
    body: &str,
    props: HashMap<String, serde_json::Value>,
) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::from(props),
    }
}

fn make_edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn make_edge_with_props(
    from_id: u64,
    to_id: u64,
    kind: &str,
    props: HashMap<String, serde_json::Value>,
) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::from(props),
    }
}

fn line_item_props(quantity: u32, line_total: f64) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("quantity".to_string(), serde_json::json!(quantity));
    p.insert("line_total".to_string(), serde_json::json!(line_total));
    p
}

fn stock_props(stock: u32) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("stock".to_string(), serde_json::json!(stock));
    p
}

fn product_props(sku: &str, price: f64) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("sku".to_string(), serde_json::json!(sku));
    p.insert("price".to_string(), serde_json::json!(price));
    p
}

fn order_props(status: &str, total: f64) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("status".to_string(), serde_json::json!(status));
    p.insert("total".to_string(), serde_json::json!(total));
    p
}

fn customer_props(tier: &str, email: &str) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("tier".to_string(), serde_json::json!(tier));
    p.insert("email".to_string(), serde_json::json!(email));
    p
}

fn warehouse_props(location: &str, capacity: u32) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("location".to_string(), serde_json::json!(location));
    p.insert("capacity".to_string(), serde_json::json!(capacity));
    p
}

fn invoice_props(status: &str, amount: f64) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("status".to_string(), serde_json::json!(status));
    p.insert("amount".to_string(), serde_json::json!(amount));
    p
}

// =========================================================================
// Domain: ERP order book
// =========================================================================

/// A complete ERP graph with all created node IDs.
///
/// ```text
/// customers: Acme Corp, Globex, Initech
/// warehouses: North, South, East
/// products: Widget, Gadget, Gizmo, Thingamajig, Doohickey
/// orders: 1001, 1002, 1003, 1004
/// invoices: INV-1, INV-2, INV-3
///
/// ordered_by:
///   1001 → Acme          1002 → Globex
///   1003 → Initech       1004 → Acme
///
/// contains (order → product):
///   1001 → Widget (qty 2), Gadget (qty 1)
///   1002 → Gizmo (qty 100)
///   1003 → Thingamajig (qty 5), Doohickey (qty 10)
///   1004 → Gadget (qty 3), Doohickey (qty 4)
///
/// stored_in (product → warehouse, stock prop):
///   Widget     → North (500), South (200)
///   Gadget     → North (300), East (100)
///   Gizmo      → South (1000)
///   Thingamajig→ East (50)
///   Doohickey  → North (100), East (50)
///
/// contains (invoice → order), billed_to (invoice → customer):
///   INV-1 contains 1001, billed_to Acme
///   INV-2 contains 1002, billed_to Globex
///   INV-3 contains 1003, billed_to Initech
/// ```
struct ErpGraph {
    // Customers
    cust_acme: u64,
    cust_globex: u64,
    cust_initech: u64,
    // Warehouses
    wh_north: u64,
    wh_south: u64,
    wh_east: u64,
    // Products
    prod_widget: u64,
    prod_gadget: u64,
    prod_gizmo: u64,
    prod_thingamajig: u64,
    prod_doohickey: u64,
    // Orders
    order_1001: u64,
    order_1002: u64,
    order_1003: u64,
    order_1004: u64,
    // Invoices
    invoice_1: u64,
    invoice_2: u64,
    invoice_3: u64,
}

fn build_erp_graph(db: &Drevo) -> ErpGraph {
    // --- Customers ---
    let cust_acme = db
        .create_node(make_node_with_props(
            "customer",
            "Acme Corp",
            "Long-time enterprise client in manufacturing",
            customer_props("enterprise", "accounts@acme.example"),
        ))
        .unwrap();
    let cust_globex = db
        .create_node(make_node_with_props(
            "customer",
            "Globex",
            "Mid-market retail chain specializing in bulk orders",
            customer_props("gold", "procurement@globex.example"),
        ))
        .unwrap();
    let cust_initech = db
        .create_node(make_node_with_props(
            "customer",
            "Initech",
            "Small business software vendor with quarterly orders",
            customer_props("silver", "ops@initech.example"),
        ))
        .unwrap();

    // --- Warehouses ---
    let wh_north = db
        .create_node(make_node_with_props(
            "warehouse",
            "North Warehouse",
            "Primary distribution center in Seattle, high capacity cold storage",
            warehouse_props("Seattle, WA", 10000),
        ))
        .unwrap();
    let wh_south = db
        .create_node(make_node_with_props(
            "warehouse",
            "South Warehouse",
            "Bulk storage facility in Austin for large-volume products",
            warehouse_props("Austin, TX", 20000),
        ))
        .unwrap();
    let wh_east = db
        .create_node(make_node_with_props(
            "warehouse",
            "East Warehouse",
            "Specialty goods facility in Boston for small-batch items",
            warehouse_props("Boston, MA", 5000),
        ))
        .unwrap();

    // --- Products ---
    let prod_widget = db
        .create_node(make_node_with_props(
            "product",
            "Widget",
            "Industrial widget used in manufacturing assemblies",
            product_props("WID-001", 19.99),
        ))
        .unwrap();
    let prod_gadget = db
        .create_node(make_node_with_props(
            "product",
            "Gadget",
            "Precision gadget with electronic components",
            product_props("GAD-002", 49.99),
        ))
        .unwrap();
    let prod_gizmo = db
        .create_node(make_node_with_props(
            "product",
            "Gizmo",
            "Bulk gizmo sold in retail packaging",
            product_props("GIZ-003", 4.99),
        ))
        .unwrap();
    let prod_thingamajig = db
        .create_node(make_node_with_props(
            "product",
            "Thingamajig",
            "Specialty thingamajig for custom applications",
            product_props("THI-004", 129.99),
        ))
        .unwrap();
    let prod_doohickey = db
        .create_node(make_node_with_props(
            "product",
            "Doohickey",
            "Small doohickey used as a replacement part",
            product_props("DOO-005", 7.49),
        ))
        .unwrap();

    // --- Orders ---
    // 1001: Widget x2 + Gadget x1 = 2*19.99 + 49.99 = 89.97
    let order_1001 = db
        .create_node(make_node_with_props(
            "order",
            "Order 1001",
            "Acme monthly replenishment order for widgets and gadgets",
            order_props("shipped", 89.97),
        ))
        .unwrap();
    // 1002: Gizmo x100 = 499.00
    let order_1002 = db
        .create_node(make_node_with_props(
            "order",
            "Order 1002",
            "Globex bulk gizmo order for holiday season",
            order_props("delivered", 499.00),
        ))
        .unwrap();
    // 1003: Thingamajig x5 + Doohickey x10 = 5*129.99 + 10*7.49 = 724.85
    let order_1003 = db
        .create_node(make_node_with_props(
            "order",
            "Order 1003",
            "Initech quarterly parts order for specialty thingamajigs",
            order_props("pending", 724.85),
        ))
        .unwrap();
    // 1004: Gadget x3 + Doohickey x4 = 3*49.99 + 4*7.49 = 179.93
    let order_1004 = db
        .create_node(make_node_with_props(
            "order",
            "Order 1004",
            "Acme follow-up order for gadget expansion project",
            order_props("pending", 179.93),
        ))
        .unwrap();

    // --- Invoices ---
    let invoice_1 = db
        .create_node(make_node_with_props(
            "invoice",
            "INV-1",
            "Invoice for Acme order 1001, net-30 terms",
            invoice_props("paid", 89.97),
        ))
        .unwrap();
    let invoice_2 = db
        .create_node(make_node_with_props(
            "invoice",
            "INV-2",
            "Invoice for Globex bulk gizmo order 1002",
            invoice_props("sent", 499.00),
        ))
        .unwrap();
    let invoice_3 = db
        .create_node(make_node_with_props(
            "invoice",
            "INV-3",
            "Invoice for Initech quarterly parts order 1003",
            invoice_props("draft", 724.85),
        ))
        .unwrap();

    // --- Edges: ordered_by (order → customer) ---
    db.create_edge(make_edge(order_1001.id, cust_acme.id, "ordered_by"))
        .unwrap();
    db.create_edge(make_edge(order_1002.id, cust_globex.id, "ordered_by"))
        .unwrap();
    db.create_edge(make_edge(order_1003.id, cust_initech.id, "ordered_by"))
        .unwrap();
    db.create_edge(make_edge(order_1004.id, cust_acme.id, "ordered_by"))
        .unwrap();

    // --- Edges: contains (order → product) with qty and line_total ---
    // Order 1001: Widget x2, Gadget x1
    db.create_edge(make_edge_with_props(
        order_1001.id,
        prod_widget.id,
        "contains",
        line_item_props(2, 39.98),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        order_1001.id,
        prod_gadget.id,
        "contains",
        line_item_props(1, 49.99),
    ))
    .unwrap();
    // Order 1002: Gizmo x100
    db.create_edge(make_edge_with_props(
        order_1002.id,
        prod_gizmo.id,
        "contains",
        line_item_props(100, 499.00),
    ))
    .unwrap();
    // Order 1003: Thingamajig x5, Doohickey x10
    db.create_edge(make_edge_with_props(
        order_1003.id,
        prod_thingamajig.id,
        "contains",
        line_item_props(5, 649.95),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        order_1003.id,
        prod_doohickey.id,
        "contains",
        line_item_props(10, 74.90),
    ))
    .unwrap();
    // Order 1004: Gadget x3, Doohickey x4
    db.create_edge(make_edge_with_props(
        order_1004.id,
        prod_gadget.id,
        "contains",
        line_item_props(3, 149.97),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        order_1004.id,
        prod_doohickey.id,
        "contains",
        line_item_props(4, 29.96),
    ))
    .unwrap();

    // --- Edges: stored_in (product → warehouse) with stock level ---
    db.create_edge(make_edge_with_props(
        prod_widget.id,
        wh_north.id,
        "stored_in",
        stock_props(500),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        prod_widget.id,
        wh_south.id,
        "stored_in",
        stock_props(200),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        prod_gadget.id,
        wh_north.id,
        "stored_in",
        stock_props(300),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        prod_gadget.id,
        wh_east.id,
        "stored_in",
        stock_props(100),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        prod_gizmo.id,
        wh_south.id,
        "stored_in",
        stock_props(1000),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        prod_thingamajig.id,
        wh_east.id,
        "stored_in",
        stock_props(50),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        prod_doohickey.id,
        wh_north.id,
        "stored_in",
        stock_props(100),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        prod_doohickey.id,
        wh_east.id,
        "stored_in",
        stock_props(50),
    ))
    .unwrap();

    // --- Edges: invoice contains order, billed_to customer ---
    db.create_edge(make_edge(invoice_1.id, order_1001.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(invoice_1.id, cust_acme.id, "billed_to"))
        .unwrap();
    db.create_edge(make_edge(invoice_2.id, order_1002.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(invoice_2.id, cust_globex.id, "billed_to"))
        .unwrap();
    db.create_edge(make_edge(invoice_3.id, order_1003.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(invoice_3.id, cust_initech.id, "billed_to"))
        .unwrap();

    ErpGraph {
        cust_acme: cust_acme.id,
        cust_globex: cust_globex.id,
        cust_initech: cust_initech.id,
        wh_north: wh_north.id,
        wh_south: wh_south.id,
        wh_east: wh_east.id,
        prod_widget: prod_widget.id,
        prod_gadget: prod_gadget.id,
        prod_gizmo: prod_gizmo.id,
        prod_thingamajig: prod_thingamajig.id,
        prod_doohickey: prod_doohickey.id,
        order_1001: order_1001.id,
        order_1002: order_1002.id,
        order_1003: order_1003.id,
        order_1004: order_1004.id,
        invoice_1: invoice_1.id,
        invoice_2: invoice_2.id,
        invoice_3: invoice_3.id,
    }
}

// =========================================================================
// Tests — run against both backends via macro
// =========================================================================

macro_rules! erp_tests {
    ($db_expr:expr, $mod_name:ident) => {
        mod $mod_name {
            use super::*;

            // -----------------------------------------------------------------
            // 1. Basic graph construction
            // -----------------------------------------------------------------

            #[test]
            fn build_complete_erp_graph() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                assert!(db.get_node(g.cust_acme).unwrap().is_some());
                assert!(db.get_node(g.cust_globex).unwrap().is_some());
                assert!(db.get_node(g.cust_initech).unwrap().is_some());
                assert!(db.get_node(g.wh_north).unwrap().is_some());
                assert!(db.get_node(g.wh_south).unwrap().is_some());
                assert!(db.get_node(g.wh_east).unwrap().is_some());
                assert!(db.get_node(g.prod_widget).unwrap().is_some());
                assert!(db.get_node(g.prod_gadget).unwrap().is_some());
                assert!(db.get_node(g.prod_gizmo).unwrap().is_some());
                assert!(db.get_node(g.prod_thingamajig).unwrap().is_some());
                assert!(db.get_node(g.prod_doohickey).unwrap().is_some());
                assert!(db.get_node(g.order_1001).unwrap().is_some());
                assert!(db.get_node(g.order_1002).unwrap().is_some());
                assert!(db.get_node(g.order_1003).unwrap().is_some());
                assert!(db.get_node(g.order_1004).unwrap().is_some());
                assert!(db.get_node(g.invoice_1).unwrap().is_some());
                assert!(db.get_node(g.invoice_2).unwrap().is_some());
                assert!(db.get_node(g.invoice_3).unwrap().is_some());
            }

            #[test]
            fn node_kinds_are_correct() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                assert_eq!(db.get_node(g.cust_acme).unwrap().unwrap().kind, "customer");
                assert_eq!(db.get_node(g.wh_north).unwrap().unwrap().kind, "warehouse");
                assert_eq!(db.get_node(g.prod_widget).unwrap().unwrap().kind, "product");
                assert_eq!(db.get_node(g.order_1001).unwrap().unwrap().kind, "order");
                assert_eq!(db.get_node(g.invoice_1).unwrap().unwrap().kind, "invoice");
            }

            #[test]
            fn product_properties_stored() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let widget = db.get_node(g.prod_widget).unwrap().unwrap();
                assert_eq!(
                    widget.properties.get("sku").unwrap(),
                    &serde_json::json!("WID-001")
                );
                assert_eq!(
                    widget.properties.get("price").unwrap(),
                    &serde_json::json!(19.99)
                );
            }

            #[test]
            fn order_properties_stored() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let order = db.get_node(g.order_1001).unwrap().unwrap();
                assert_eq!(
                    order.properties.get("status").unwrap(),
                    &serde_json::json!("shipped")
                );
                assert_eq!(
                    order.properties.get("total").unwrap(),
                    &serde_json::json!(89.97)
                );
            }

            #[test]
            fn customer_properties_stored() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let acme = db.get_node(g.cust_acme).unwrap().unwrap();
                assert_eq!(
                    acme.properties.get("tier").unwrap(),
                    &serde_json::json!("enterprise")
                );
                assert_eq!(
                    acme.properties.get("email").unwrap(),
                    &serde_json::json!("accounts@acme.example")
                );
            }

            #[test]
            fn warehouse_properties_stored() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let north = db.get_node(g.wh_north).unwrap().unwrap();
                assert_eq!(
                    north.properties.get("location").unwrap(),
                    &serde_json::json!("Seattle, WA")
                );
                assert_eq!(
                    north.properties.get("capacity").unwrap(),
                    &serde_json::json!(10000)
                );
            }

            // -----------------------------------------------------------------
            // 2. Order → Customer (ordered_by)
            // -----------------------------------------------------------------

            #[test]
            fn order_to_customer() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let customer = db
                    .neighbors(g.order_1001, Direction::Outgoing, Some("ordered_by"))
                    .unwrap();
                assert_eq!(customer.len(), 1);
                assert_eq!(customer[0].id, g.cust_acme);
            }

            #[test]
            fn customer_has_multiple_orders() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Acme has 2 orders: 1001 and 1004
                let orders = db
                    .neighbors(g.cust_acme, Direction::Incoming, Some("ordered_by"))
                    .unwrap();
                assert_eq!(orders.len(), 2);
                let ids: Vec<u64> = orders.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.order_1001));
                assert!(ids.contains(&g.order_1004));
            }

            #[test]
            fn customer_with_single_order() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let orders = db
                    .neighbors(g.cust_globex, Direction::Incoming, Some("ordered_by"))
                    .unwrap();
                assert_eq!(orders.len(), 1);
                assert_eq!(orders[0].id, g.order_1002);
            }

            // -----------------------------------------------------------------
            // 3. Order → Product (contains, line items)
            // -----------------------------------------------------------------

            #[test]
            fn order_line_items() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Order 1001 contains Widget and Gadget
                let items = db
                    .neighbors(g.order_1001, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(items.len(), 2);
                let ids: Vec<u64> = items.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.prod_widget));
                assert!(ids.contains(&g.prod_gadget));
            }

            #[test]
            fn bulk_order_single_product() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let items = db
                    .neighbors(g.order_1002, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].id, g.prod_gizmo);
            }

            #[test]
            fn product_ordered_in_multiple_orders() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Gadget appears in orders 1001 and 1004
                let orders = db
                    .neighbors(g.prod_gadget, Direction::Incoming, Some("contains"))
                    .unwrap();
                // Both orders and (no invoice contains this product) → 2 incoming contains
                let order_ids: Vec<u64> = orders
                    .iter()
                    .filter(|n| n.kind == "order")
                    .map(|n| n.id)
                    .collect();
                assert_eq!(order_ids.len(), 2);
                assert!(order_ids.contains(&g.order_1001));
                assert!(order_ids.contains(&g.order_1004));
            }

            #[test]
            fn line_item_quantities() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Check order 1002 has Gizmo with qty=100
                let edges = db.edges_of(g.order_1002, Direction::Outgoing).unwrap();
                let gizmo_line = edges
                    .iter()
                    .find(|e| e.kind == "contains" && e.to_id == g.prod_gizmo)
                    .unwrap();
                assert_eq!(
                    gizmo_line.properties.get("quantity").unwrap(),
                    &serde_json::json!(100)
                );
                assert_eq!(
                    gizmo_line.properties.get("line_total").unwrap(),
                    &serde_json::json!(499.00)
                );
            }

            #[test]
            fn sum_order_line_items_matches_total() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let edges = db.edges_of(g.order_1003, Direction::Outgoing).unwrap();
                let sum: f64 = edges
                    .iter()
                    .filter(|e| e.kind == "contains")
                    .map(|e| {
                        e.properties
                            .get("line_total")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                    })
                    .sum();
                // 5*129.99 + 10*7.49 = 649.95 + 74.90 = 724.85
                assert!((sum - 724.85).abs() < 0.001);

                let order = db.get_node(g.order_1003).unwrap().unwrap();
                let total = order.properties.get("total").unwrap().as_f64().unwrap();
                assert!((sum - total).abs() < 0.001);
            }

            // -----------------------------------------------------------------
            // 4. Product → Warehouse (stored_in, inventory)
            // -----------------------------------------------------------------

            #[test]
            fn product_stored_in_single_warehouse() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let warehouses = db
                    .neighbors(g.prod_gizmo, Direction::Outgoing, Some("stored_in"))
                    .unwrap();
                assert_eq!(warehouses.len(), 1);
                assert_eq!(warehouses[0].id, g.wh_south);
            }

            #[test]
            fn product_stored_in_multiple_warehouses() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let warehouses = db
                    .neighbors(g.prod_widget, Direction::Outgoing, Some("stored_in"))
                    .unwrap();
                assert_eq!(warehouses.len(), 2);
                let ids: Vec<u64> = warehouses.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.wh_north));
                assert!(ids.contains(&g.wh_south));
            }

            #[test]
            fn warehouse_stocks_multiple_products() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // North warehouse has Widget, Gadget, Doohickey
                let products = db
                    .neighbors(g.wh_north, Direction::Incoming, Some("stored_in"))
                    .unwrap();
                assert_eq!(products.len(), 3);
                let ids: Vec<u64> = products.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.prod_widget));
                assert!(ids.contains(&g.prod_gadget));
                assert!(ids.contains(&g.prod_doohickey));
            }

            #[test]
            fn stock_level_stored_on_edge() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let edges = db.edges_of(g.prod_widget, Direction::Outgoing).unwrap();
                let north_stock = edges
                    .iter()
                    .find(|e| e.kind == "stored_in" && e.to_id == g.wh_north)
                    .unwrap();
                assert_eq!(
                    north_stock.properties.get("stock").unwrap(),
                    &serde_json::json!(500)
                );
            }

            #[test]
            fn total_stock_across_warehouses() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Widget: 500 + 200 = 700
                let edges = db.edges_of(g.prod_widget, Direction::Outgoing).unwrap();
                let total_stock: u64 = edges
                    .iter()
                    .filter(|e| e.kind == "stored_in")
                    .map(|e| {
                        e.properties
                            .get("stock")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                    })
                    .sum();
                assert_eq!(total_stock, 700);
            }

            // -----------------------------------------------------------------
            // 5. Invoice → Order → Customer (billing chains)
            // -----------------------------------------------------------------

            #[test]
            fn invoice_contains_order() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let orders = db
                    .neighbors(g.invoice_1, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(orders.len(), 1);
                assert_eq!(orders[0].id, g.order_1001);
            }

            #[test]
            fn invoice_billed_to_customer() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let customer = db
                    .neighbors(g.invoice_1, Direction::Outgoing, Some("billed_to"))
                    .unwrap();
                assert_eq!(customer.len(), 1);
                assert_eq!(customer[0].id, g.cust_acme);
            }

            #[test]
            fn customer_receives_invoices() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let invoices = db
                    .neighbors(g.cust_acme, Direction::Incoming, Some("billed_to"))
                    .unwrap();
                assert_eq!(invoices.len(), 1);
                assert_eq!(invoices[0].id, g.invoice_1);
            }

            #[test]
            fn order_without_invoice() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Order 1004 has no invoice yet (contains incoming must be empty)
                let incoming = db
                    .neighbors(g.order_1004, Direction::Incoming, Some("contains"))
                    .unwrap();
                assert_eq!(incoming.len(), 0);
            }

            // -----------------------------------------------------------------
            // 6. Kind index — board views
            // -----------------------------------------------------------------

            #[test]
            fn kind_index_lists_all_customers() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let customers = db.list_nodes_by_kind("customer", 100, 0).unwrap();
                assert_eq!(customers.len(), 3);
            }

            #[test]
            fn kind_index_lists_all_warehouses() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let warehouses = db.list_nodes_by_kind("warehouse", 100, 0).unwrap();
                assert_eq!(warehouses.len(), 3);
            }

            #[test]
            fn kind_index_lists_all_products() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let products = db.list_nodes_by_kind("product", 100, 0).unwrap();
                assert_eq!(products.len(), 5);
            }

            #[test]
            fn kind_index_lists_all_orders() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let orders = db.list_nodes_by_kind("order", 100, 0).unwrap();
                assert_eq!(orders.len(), 4);
            }

            #[test]
            fn kind_index_lists_all_invoices() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let invoices = db.list_nodes_by_kind("invoice", 100, 0).unwrap();
                assert_eq!(invoices.len(), 3);
            }

            #[test]
            fn kind_index_pagination() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let page1 = db.list_nodes_by_kind("product", 2, 0).unwrap();
                assert_eq!(page1.len(), 2);
                let page2 = db.list_nodes_by_kind("product", 2, 2).unwrap();
                assert_eq!(page2.len(), 2);
                let page3 = db.list_nodes_by_kind("product", 2, 4).unwrap();
                assert_eq!(page3.len(), 1);
            }

            // -----------------------------------------------------------------
            // 7. FTS — search by name / description
            // -----------------------------------------------------------------

            #[test]
            fn fts_find_product_by_name() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let results = db.search_fts("Widget", 10).unwrap();
                assert!(!results.is_empty());
                let titles: Vec<&str> = results.iter().map(|r| r.node.title.as_str()).collect();
                assert!(titles.contains(&"Widget"));
            }

            #[test]
            fn fts_find_product_by_description() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let results = db.search_fts("manufacturing", 10).unwrap();
                assert!(!results.is_empty());
                // Widget body mentions "manufacturing assemblies"
                let found = results.iter().any(|r| r.node.title == "Widget");
                assert!(found);
            }

            #[test]
            fn fts_find_warehouse_by_location() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let results = db.search_fts("Seattle", 10).unwrap();
                assert!(!results.is_empty());
                let found = results.iter().any(|r| r.node.title == "North Warehouse");
                assert!(found);
            }

            #[test]
            fn fts_find_customer_by_name() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let results = db.search_fts("Globex", 10).unwrap();
                assert!(!results.is_empty());
                let titles: Vec<&str> = results.iter().map(|r| r.node.title.as_str()).collect();
                assert!(titles.contains(&"Globex"));
            }

            #[test]
            fn fts_find_order_by_description() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let results = db.search_fts("holiday", 10).unwrap();
                assert!(!results.is_empty());
                // Order 1002 body mentions "holiday season"
                let found = results.iter().any(|r| r.node.title == "Order 1002");
                assert!(found);
            }

            // -----------------------------------------------------------------
            // 8. Subgraph — full context of an order
            // -----------------------------------------------------------------

            #[test]
            fn subgraph_of_order_depth_1() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Depth 1 around order 1001: customer (ordered_by), line items (contains),
                // and incoming invoice (contains from invoice_1).
                let sg = db.subgraph(g.order_1001, 1).unwrap();
                let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                assert!(ids.contains(&g.order_1001));
                assert!(ids.contains(&g.cust_acme));
                assert!(ids.contains(&g.prod_widget));
                assert!(ids.contains(&g.prod_gadget));
                assert!(ids.contains(&g.invoice_1));
            }

            #[test]
            fn subgraph_of_order_depth_2_includes_warehouses() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Depth 2 adds warehouses that stock the line-item products
                let sg = db.subgraph(g.order_1001, 2).unwrap();
                let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                assert!(ids.contains(&g.order_1001));
                // Widget stored in North + South
                assert!(ids.contains(&g.wh_north));
                assert!(ids.contains(&g.wh_south));
                // Gadget stored in North + East
                assert!(ids.contains(&g.wh_east));
            }

            #[test]
            fn subgraph_of_customer_sees_orders_and_invoices() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Depth 1 from Acme: incoming orders (ordered_by) + incoming invoices (billed_to)
                let sg = db.subgraph(g.cust_acme, 1).unwrap();
                let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                assert!(ids.contains(&g.cust_acme));
                assert!(ids.contains(&g.order_1001));
                assert!(ids.contains(&g.order_1004));
                assert!(ids.contains(&g.invoice_1));
            }

            // -----------------------------------------------------------------
            // 9. Traversals — BFS / DFS / shortest path
            // -----------------------------------------------------------------

            #[test]
            fn bfs_from_order_all_edges() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // BFS depth 1 from order 1001, all outgoing edges
                let neighbors = db.bfs(g.order_1001, 1, Direction::Outgoing, None).unwrap();
                let ids: Vec<u64> = neighbors.iter().map(|n| n.id).collect();
                // customer + 2 products
                assert!(ids.contains(&g.cust_acme));
                assert!(ids.contains(&g.prod_widget));
                assert!(ids.contains(&g.prod_gadget));
            }

            #[test]
            fn bfs_multi_hop_to_warehouses() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // BFS depth 2 from order 1001 outgoing: line items (depth 1) and their warehouses (depth 2)
                let reached = db.bfs(g.order_1001, 2, Direction::Outgoing, None).unwrap();
                let ids: Vec<u64> = reached.iter().map(|n| n.id).collect();
                // Widget's warehouses: North, South
                assert!(ids.contains(&g.wh_north));
                assert!(ids.contains(&g.wh_south));
                // Gadget's warehouses: North, East
                assert!(ids.contains(&g.wh_east));
            }

            #[test]
            fn bfs_depth_limited() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Depth 1 from order 1001 should NOT reach warehouses
                let reached = db.bfs(g.order_1001, 1, Direction::Outgoing, None).unwrap();
                let ids: Vec<u64> = reached.iter().map(|n| n.id).collect();
                assert!(!ids.contains(&g.wh_north));
                assert!(!ids.contains(&g.wh_south));
                assert!(!ids.contains(&g.wh_east));
            }

            #[test]
            fn dfs_from_order_products_only() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let products = db
                    .dfs(g.order_1001, 10, Direction::Outgoing, Some("contains"))
                    .unwrap();
                let ids: Vec<u64> = products.iter().map(|n| n.id).collect();
                assert_eq!(products.len(), 2);
                assert!(ids.contains(&g.prod_widget));
                assert!(ids.contains(&g.prod_gadget));
            }

            #[test]
            fn shortest_path_customer_to_warehouse() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Acme → (incoming ordered_by) order 1001 → (contains) Widget → (stored_in) North
                // But shortest_path uses outgoing direction only.
                // From order_1001 → Widget → North we can compute.
                let path = db.shortest_path(g.order_1001, g.wh_north).unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                // order_1001 → widget → wh_north  (3 nodes)
                assert_eq!(path.len(), 3);
                assert_eq!(path[0], g.order_1001);
                assert_eq!(path[2], g.wh_north);
            }

            #[test]
            fn shortest_path_order_to_customer() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let path = db.shortest_path(g.order_1002, g.cust_globex).unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                assert_eq!(path.len(), 2);
                assert_eq!(path[0], g.order_1002);
                assert_eq!(path[1], g.cust_globex);
            }

            #[test]
            fn shortest_path_invoice_to_warehouse() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // invoice_2 → order_1002 → gizmo → wh_south
                let path = db.shortest_path(g.invoice_2, g.wh_south).unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                assert_eq!(path.len(), 4);
                assert_eq!(path[0], g.invoice_2);
                assert_eq!(path[3], g.wh_south);
            }

            // -----------------------------------------------------------------
            // 10. Edge kind index
            // -----------------------------------------------------------------

            #[test]
            fn edge_kind_index_ordered_by() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let edges = db.list_edges_by_kind("ordered_by", 100, 0).unwrap();
                assert_eq!(edges.len(), 4); // 4 orders each ordered_by 1 customer
            }

            #[test]
            fn edge_kind_index_contains() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                // Line items: 2 + 1 + 2 + 2 = 7, plus 3 invoice→order contains = 10
                let edges = db.list_edges_by_kind("contains", 100, 0).unwrap();
                assert_eq!(edges.len(), 10);
            }

            #[test]
            fn edge_kind_index_stored_in() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                // Widget 2 + Gadget 2 + Gizmo 1 + Thingamajig 1 + Doohickey 2 = 8
                let edges = db.list_edges_by_kind("stored_in", 100, 0).unwrap();
                assert_eq!(edges.len(), 8);
            }

            #[test]
            fn edge_kind_index_billed_to() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let edges = db.list_edges_by_kind("billed_to", 100, 0).unwrap();
                assert_eq!(edges.len(), 3);
            }

            // -----------------------------------------------------------------
            // 11. Transactional updates — order status, inventory
            // -----------------------------------------------------------------

            #[test]
            fn update_order_status_to_shipped() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let mut new_props = HashMap::new();
                new_props.insert("status".to_string(), serde_json::json!("shipped"));
                new_props.insert("total".to_string(), serde_json::json!(724.85));

                let updated = db
                    .update_node(
                        g.order_1003,
                        NodePatch {
                            kind: None,
                            title: None,
                            body: None,
                            body_html: None,
                            properties: Some(Properties::from(new_props)),
                        },
                    )
                    .unwrap();

                assert_eq!(
                    updated.properties.get("status").unwrap(),
                    &serde_json::json!("shipped")
                );
            }

            #[test]
            fn update_stock_level_on_warehouse_edge() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Simulate an inventory decrement: Widget at North goes from 500 → 498 (after Order 1001 ships)
                let edges = db.edges_of(g.prod_widget, Direction::Outgoing).unwrap();
                let north_edge = edges
                    .iter()
                    .find(|e| e.kind == "stored_in" && e.to_id == g.wh_north)
                    .unwrap();

                let mut new_props = HashMap::new();
                new_props.insert("stock".to_string(), serde_json::json!(498));

                let updated = db
                    .update_edge(
                        north_edge.id,
                        EdgePatch {
                            kind: None,
                            weight: None,
                            properties: Some(Properties::from(new_props)),
                        },
                    )
                    .unwrap();

                assert_eq!(
                    updated.properties.get("stock").unwrap(),
                    &serde_json::json!(498)
                );
            }

            #[test]
            fn add_new_order_to_existing_customer() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let new_order = db
                    .create_node(make_node_with_props(
                        "order",
                        "Order 1005",
                        "Globex follow-up gizmo order",
                        order_props("pending", 249.50),
                    ))
                    .unwrap();

                db.create_edge(make_edge(new_order.id, g.cust_globex, "ordered_by"))
                    .unwrap();
                db.create_edge(make_edge_with_props(
                    new_order.id,
                    g.prod_gizmo,
                    "contains",
                    line_item_props(50, 249.50),
                ))
                .unwrap();

                // Globex now has 2 orders
                let orders = db
                    .neighbors(g.cust_globex, Direction::Incoming, Some("ordered_by"))
                    .unwrap();
                assert_eq!(orders.len(), 2);
            }

            #[test]
            fn restock_product_to_new_warehouse() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Gizmo restocked at North warehouse
                db.create_edge(make_edge_with_props(
                    g.prod_gizmo,
                    g.wh_north,
                    "stored_in",
                    stock_props(250),
                ))
                .unwrap();

                let warehouses = db
                    .neighbors(g.prod_gizmo, Direction::Outgoing, Some("stored_in"))
                    .unwrap();
                assert_eq!(warehouses.len(), 2);
            }

            // -----------------------------------------------------------------
            // 12. Cascade delete
            // -----------------------------------------------------------------

            #[test]
            fn delete_product_cascades_edges() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                db.delete_node(g.prod_doohickey).unwrap();

                assert!(db.get_node(g.prod_doohickey).unwrap().is_none());

                // Order 1003 now contains only thingamajig
                let items = db
                    .neighbors(g.order_1003, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].id, g.prod_thingamajig);

                // Order 1004 now contains only gadget
                let items4 = db
                    .neighbors(g.order_1004, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(items4.len(), 1);
                assert_eq!(items4[0].id, g.prod_gadget);

                // North warehouse no longer has doohickey stocked
                let north_products = db
                    .neighbors(g.wh_north, Direction::Incoming, Some("stored_in"))
                    .unwrap();
                assert!(!north_products.iter().any(|n| n.id == g.prod_doohickey));
            }

            #[test]
            fn delete_order_cascades_edges() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                db.delete_node(g.order_1001).unwrap();
                assert!(db.get_node(g.order_1001).unwrap().is_none());

                // Acme now has 1 remaining order (1004)
                let acme_orders = db
                    .neighbors(g.cust_acme, Direction::Incoming, Some("ordered_by"))
                    .unwrap();
                assert_eq!(acme_orders.len(), 1);
                assert_eq!(acme_orders[0].id, g.order_1004);

                // Invoice 1 no longer contains any order
                let inv1_orders = db
                    .neighbors(g.invoice_1, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(inv1_orders.len(), 0);
            }

            #[test]
            fn cancel_order_via_edge_deletion() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Remove line items from order 1004 (cancel order)
                let edges = db.edges_of(g.order_1004, Direction::Outgoing).unwrap();
                for e in edges.iter().filter(|e| e.kind == "contains") {
                    db.delete_edge(e.id).unwrap();
                }

                // Order still exists but has no line items
                assert!(db.get_node(g.order_1004).unwrap().is_some());
                let items = db
                    .neighbors(g.order_1004, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(items.len(), 0);
            }

            // -----------------------------------------------------------------
            // 13. Advanced queries
            // -----------------------------------------------------------------

            #[test]
            fn find_orders_by_status() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let orders = db.list_nodes_by_kind("order", 100, 0).unwrap();
                let pending: Vec<&Node> = orders
                    .iter()
                    .filter(|o| {
                        o.properties
                            .get("status")
                            .map(|v| v == &serde_json::json!("pending"))
                            .unwrap_or(false)
                    })
                    .collect();
                // Order 1003 and 1004 are pending
                assert_eq!(pending.len(), 2);
            }

            #[test]
            fn find_customers_without_orders() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // All customers here have orders, but let's add one without
                let new_cust = db
                    .create_node(make_node_with_props(
                        "customer",
                        "Soylent",
                        "Potential new customer, no orders yet",
                        customer_props("lead", "contact@soylent.example"),
                    ))
                    .unwrap();

                let customers = db.list_nodes_by_kind("customer", 100, 0).unwrap();
                let without_orders: Vec<&Node> = customers
                    .iter()
                    .filter(|c| {
                        db.neighbors(c.id, Direction::Incoming, Some("ordered_by"))
                            .unwrap()
                            .is_empty()
                    })
                    .collect();
                assert_eq!(without_orders.len(), 1);
                assert_eq!(without_orders[0].id, new_cust.id);

                // Silence unused warning
                let _ = g.cust_acme;
            }

            #[test]
            fn total_inventory_value_per_warehouse() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // For North: compute sum of stock * product.price across all stored products
                let stored_edges = db.edges_of(g.wh_north, Direction::Incoming).unwrap();
                let mut total_value = 0.0;
                for edge in stored_edges.iter().filter(|e| e.kind == "stored_in") {
                    let stock = edge
                        .properties
                        .get("stock")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as f64;
                    let product = db.get_node(edge.from_id).unwrap().unwrap();
                    let price = product
                        .properties
                        .get("price")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    total_value += stock * price;
                }
                // Widget 500*19.99 + Gadget 300*49.99 + Doohickey 100*7.49
                // = 9995 + 14997 + 749 = 25741
                assert!((total_value - 25741.0).abs() < 0.001);
            }

            // -----------------------------------------------------------------
            // 14. Node lookup
            // -----------------------------------------------------------------

            #[test]
            fn find_customer_by_title() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let node = db.get_node_by_title("Acme Corp").unwrap();
                assert!(node.is_some());
                assert_eq!(node.unwrap().id, g.cust_acme);
            }

            #[test]
            fn find_product_by_uuid() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let original = db.get_node(g.prod_widget).unwrap().unwrap();
                let by_uuid = db.get_node_by_uuid(&original.uuid).unwrap();
                assert!(by_uuid.is_some());
                assert_eq!(by_uuid.unwrap().id, g.prod_widget);
            }

            #[test]
            fn list_recent_returns_latest() {
                let db = $db_expr;
                let _g = build_erp_graph(&db);

                let recent = db.list_recent(5).unwrap();
                assert_eq!(recent.len(), 5);
            }

            // -----------------------------------------------------------------
            // 15. Edge direction filtering
            // -----------------------------------------------------------------

            #[test]
            fn edges_of_order_both_directions() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Order 1001: outgoing = 2 contains + 1 ordered_by, incoming = 1 contains (from invoice)
                let all = db.edges_of(g.order_1001, Direction::Both).unwrap();
                assert_eq!(all.len(), 4);
            }

            #[test]
            fn edges_of_order_outgoing_only() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let out = db.edges_of(g.order_1001, Direction::Outgoing).unwrap();
                assert_eq!(out.len(), 3); // 2 contains + 1 ordered_by
                assert!(out.iter().all(|e| e.from_id == g.order_1001));
            }

            #[test]
            fn edges_of_order_incoming_only() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                let incoming = db.edges_of(g.order_1001, Direction::Incoming).unwrap();
                assert_eq!(incoming.len(), 1); // contains from invoice_1
                assert!(incoming.iter().all(|e| e.to_id == g.order_1001));
            }

            // -----------------------------------------------------------------
            // 16. Edge weight on priority
            // -----------------------------------------------------------------

            #[test]
            fn update_stored_in_edge_weight_for_priority() {
                let db = $db_expr;
                let g = build_erp_graph(&db);

                // Bump weight on Widget→North to mark as primary warehouse
                let edges = db.edges_of(g.prod_widget, Direction::Outgoing).unwrap();
                let north_edge = edges
                    .iter()
                    .find(|e| e.kind == "stored_in" && e.to_id == g.wh_north)
                    .unwrap();

                let updated = db
                    .update_edge(
                        north_edge.id,
                        EdgePatch {
                            kind: None,
                            weight: Some(10.0),
                            properties: None,
                        },
                    )
                    .unwrap();
                assert_eq!(updated.weight, 10.0);
            }
        }
    };
}

// Run all tests against MemoryBackend
erp_tests!(memory_db(), memory);

// Run a smoke-test subset against RedbBackend
#[cfg(feature = "redb-backend")]
mod redb {
    use super::*;

    macro_rules! erp_redb_tests {
        ($mod_name:ident) => {
            mod $mod_name {
                use super::*;
                use tempfile::TempDir;

                fn setup() -> (TempDir, Drevo) {
                    let dir = TempDir::new().expect("create temp dir");
                    let db = redb_db(&dir);
                    (dir, db)
                }

                #[test]
                fn build_complete_erp_graph() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    assert!(db.get_node(g.cust_acme).unwrap().is_some());
                    assert!(db.get_node(g.invoice_3).unwrap().is_some());
                }

                #[test]
                fn customer_orders_view() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    let orders = db
                        .neighbors(g.cust_acme, Direction::Incoming, Some("ordered_by"))
                        .unwrap();
                    assert_eq!(orders.len(), 2);
                }

                #[test]
                fn order_line_items() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    let items = db
                        .neighbors(g.order_1001, Direction::Outgoing, Some("contains"))
                        .unwrap();
                    assert_eq!(items.len(), 2);
                }

                #[test]
                fn product_warehouse_distribution() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    let warehouses = db
                        .neighbors(g.prod_widget, Direction::Outgoing, Some("stored_in"))
                        .unwrap();
                    assert_eq!(warehouses.len(), 2);
                }

                #[test]
                fn invoice_billing_chain() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    let customer = db
                        .neighbors(g.invoice_1, Direction::Outgoing, Some("billed_to"))
                        .unwrap();
                    assert_eq!(customer.len(), 1);
                    assert_eq!(customer[0].id, g.cust_acme);
                }

                #[test]
                fn kind_index_products() {
                    let (_dir, db) = setup();
                    let _g = build_erp_graph(&db);
                    let products = db.list_nodes_by_kind("product", 100, 0).unwrap();
                    assert_eq!(products.len(), 5);
                }

                #[test]
                fn fts_search_product() {
                    let (_dir, db) = setup();
                    let _g = build_erp_graph(&db);
                    let results = db.search_fts("Gizmo", 10).unwrap();
                    assert!(!results.is_empty());
                }

                #[test]
                fn shortest_path_order_to_warehouse() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    let path = db.shortest_path(g.order_1002, g.wh_south).unwrap();
                    assert!(path.is_some());
                    assert_eq!(path.unwrap().len(), 3);
                }

                #[test]
                fn subgraph_extraction() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    let sg = db.subgraph(g.order_1001, 1).unwrap();
                    let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();
                    assert!(ids.contains(&g.cust_acme));
                    assert!(ids.contains(&g.prod_widget));
                }

                #[test]
                fn delete_product_cascades() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    db.delete_node(g.prod_doohickey).unwrap();
                    let items = db
                        .neighbors(g.order_1003, Direction::Outgoing, Some("contains"))
                        .unwrap();
                    assert_eq!(items.len(), 1);
                }

                #[test]
                fn edge_properties_persist() {
                    let (_dir, db) = setup();
                    let g = build_erp_graph(&db);
                    let edges = db.edges_of(g.prod_widget, Direction::Outgoing).unwrap();
                    let north_edge = edges
                        .iter()
                        .find(|e| e.kind == "stored_in" && e.to_id == g.wh_north)
                        .unwrap();
                    assert_eq!(
                        north_edge.properties.get("stock").unwrap(),
                        &serde_json::json!(500)
                    );
                }
            }
        };
    }

    erp_redb_tests!(redb_backend);
}
