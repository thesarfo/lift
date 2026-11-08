//! Write a synthetic mongodump for examples/shop.toml, for benchmarks and demos.
//!
//!     cargo run --release --example gen_shop -- OUT_DIR ORDERS
//!
//! Writes OUT_DIR/shop/{customers,products,orders}.bson. About 360 bytes per order, so
//! 14 million orders is roughly 5 GB. The data is a little messy on purpose, like real data:
//! 1 in 1000 orders points at a customer that doesn't exist, and 1 in 50 line items has its
//! quantity stored as a string.

use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
};

use bson::{DateTime, Document, doc, oid::ObjectId};

/// xorshift64*: deterministic, so the same arguments always give the same dump.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// ObjectIds from a counter, so customers and products can be referenced by index.
fn oid(kind: u8, n: u64) -> ObjectId {
    let mut b = [0u8; 12];
    b[0..4].copy_from_slice(&1_700_000_000u32.to_be_bytes());
    b[4] = kind;
    b[5..12].copy_from_slice(&n.to_be_bytes()[1..]);
    ObjectId::from_bytes(b)
}

fn writer(dir: &Path, name: &str) -> BufWriter<File> {
    BufWriter::with_capacity(1 << 20, File::create(dir.join(format!("{name}.bson"))).expect("create file"))
}

fn put(w: &mut impl Write, d: &Document) {
    d.to_writer(w).expect("write bson");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, orders] = args.as_slice() else {
        eprintln!("usage: gen_shop OUT_DIR ORDERS");
        std::process::exit(2);
    };
    let orders: u64 = orders.parse().expect("ORDERS must be a number");
    let customers = (orders / 4).max(1);
    let products = 10_000u64;
    let dir = Path::new(out).join("shop");
    fs::create_dir_all(&dir).expect("create dump dir");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let epoch_ms = 1_700_000_000_000i64;

    let mut w = writer(&dir, "products");
    for i in 0..products {
        put(&mut w, &doc! {
            "_id": oid(1, i),
            "sku": format!("SKU-{i:06}"),
            "name": format!("Product {i}"),
            "price": (rng.below(100_000) as f64) / 100.0,
        });
    }
    w.flush().unwrap();

    let mut w = writer(&dir, "customers");
    for i in 0..customers {
        let at = DateTime::from_millis(epoch_ms + rng.below(31_536_000_000) as i64);
        put(&mut w, &doc! {
            "_id": oid(2, i),
            "name": format!("Customer {i}"),
            "email": format!("customer{i}@example.com"),
            "meta": { "created_at": at, "updated_at": at },
        });
    }
    w.flush().unwrap();

    let statuses = ["placed", "paid", "shipped", "delivered", "cancelled"];
    let mut w = writer(&dir, "orders");
    for i in 0..orders {
        let customer = if rng.below(1000) == 0 { oid(9, i) } else { oid(2, rng.below(customers)) };
        let items: Vec<Document> = (0..1 + rng.below(5))
            .map(|_| {
                let qty = 1 + rng.below(9) as i32;
                doc! {
                    "product_id": oid(1, rng.below(products)),
                    "qty": if rng.below(50) == 0 { bson::Bson::String(qty.to_string()) } else { qty.into() },
                    "price": (rng.below(100_000) as f64) / 100.0,
                }
            })
            .collect();
        let at = DateTime::from_millis(epoch_ms + rng.below(31_536_000_000) as i64);
        put(&mut w, &doc! {
            "_id": oid(3, i),
            "customer_id": customer,
            "status": statuses[rng.below(5) as usize],
            "currency": "GHS",
            "placed_at": at,
            "items": items,
            "meta": { "created_at": at, "updated_at": at },
        });
    }
    w.flush().unwrap();
    eprintln!("wrote {products} products, {customers} customers, {orders} orders to {}", dir.display());
}
