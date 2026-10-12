//! The weather DDL read into tables, the table's orders and their indexes.

mod weather;

use warren_snail::catalog::{Catalog, ColumnType, Level};
use weather::DDL;

#[test]
fn the_weather_ddl_declares_its_orders_and_their_indexes() {
    let c = Catalog::parse(DDL).unwrap();
    let names: Vec<&str> = c.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["weather.stations", "weather.readings"]);

    // With no CLUSTER ON, the primary key is the table's order; the index on the name is an order
    // of its own, ending in the table's order's key.
    let stations = c.table("weather.stations").unwrap();
    assert_eq!(stations.orders.len(), 2);
    assert_eq!(stations.orders[0].name, "stations_pkey");
    assert!(stations.orders[0].table_order && stations.orders[0].unique);
    assert_eq!(
        stations.orders[0].levels,
        [Level::Column(0), Level::Column(1)]
    );
    assert_eq!(stations.orders[0].columns, [0, 1, 2]);
    assert_eq!(stations.orders[1].name, "s_name");
    assert!(!stations.orders[1].table_order);
    assert_eq!(
        stations.orders[1].levels,
        [Level::Column(2), Level::Column(0), Level::Column(1)]
    );
    assert_eq!(stations.columns[2].ty, ColumnType::Text(Some(80)));

    // CLUSTER ON names the readings' order; the primary key and the month are orders of their own,
    // the month read through the schema's clock and carrying what INCLUDE names.
    let r = c.table("weather.readings").unwrap();
    let col = |n: &str| r.column(n).unwrap();
    let orders: Vec<&str> = r.orders.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(orders, ["r_station", "readings_pkey", "r_month"]);
    assert!(r.orders[0].table_order && !r.orders[0].unique);
    assert_eq!(
        r.orders[0].levels,
        [
            Level::Column(col("network_id")),
            Level::Column(col("station_no"))
        ]
    );
    assert_eq!(r.orders[0].columns, [0, 1, 2, 3, 4, 5]);
    assert!(!r.orders[1].table_order && r.orders[1].unique);
    assert_eq!(r.orders[1].levels, [Level::Column(col("reading_id"))]);
    assert_eq!(
        r.orders[1].columns,
        [col("reading_id"), col("network_id"), col("station_no")]
    );
    assert_eq!(
        r.orders[2].levels,
        [
            Level::MonthUtc(col("observed_at")),
            Level::Column(col("observed_at"))
        ]
    );
    assert_eq!(
        r.orders[2].columns,
        [
            col("observed_at"),
            col("checked_at"),
            col("network_id"),
            col("station_no")
        ]
    );
    assert_eq!(r.columns[col("feed")].ty, ColumnType::Text(Some(32)));
    assert_eq!(r.month_columns(), [col("observed_at")]);
    assert!(r.columns[col("observed_at")].not_null);
    assert!(!r.columns[col("checked_at")].not_null);
}
