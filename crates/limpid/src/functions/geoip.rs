//! geoip: MaxMind GeoIP database lookup.
//!
//! Used as an expression function in DSL:
//!   `fields.geo = geoip(fields.src_ip)`
//!
//! Returns an object: `{ country, city, latitude, longitude }`

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Result;
use maxminddb::{Reader, geoip2};
use serde_json::Value;

/// Global GeoIP database reader (initialized once at startup).
static GEOIP_DB: OnceLock<Option<Reader<Vec<u8>>>> = OnceLock::new();

/// Initialize the GeoIP database from a file path.
/// Call this once at startup. If the path is None or the file doesn't exist,
/// geoip lookups will return an error (catchable via try/catch).
pub fn init(path: Option<&PathBuf>) {
    GEOIP_DB.get_or_init(|| {
        path.and_then(|p| {
            Reader::open_readfile(p)
                .inspect_err(|e| tracing::warn!("failed to open GeoIP database: {}", e))
                .ok()
        })
    });
}

/// Look up an IP address and return a JSON object with geo-location data.
///
/// Returns: `{ "country": "JP", "city": "Tokyo", "latitude": 35.6, "longitude": 139.7 }`
/// Fields that are unavailable are omitted (not null).
pub fn lookup(ip_str: &str) -> Result<Value> {
    let reader = GEOIP_DB
        .get()
        .and_then(|opt| opt.as_ref())
        .ok_or_else(|| anyhow::anyhow!("GeoIP database not loaded"))?;

    let ip: IpAddr = ip_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid IP '{}': {}", ip_str, e))?;

    let result = reader
        .lookup(ip)
        .map_err(|e| anyhow::anyhow!("GeoIP lookup failed: {}", e))?;
    let city: geoip2::City = result
        .decode()
        .map_err(|e| anyhow::anyhow!("GeoIP decode failed: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("GeoIP: no data for IP {}", ip_str))?;

    let mut map = serde_json::Map::new();

    if let Some(iso) = city.country.iso_code {
        map.insert("country".into(), Value::String(iso.to_string()));
    }

    if let Some(name) = city.city.names.english {
        map.insert("city".into(), Value::String(name.to_string()));
    }

    if let Some(lat) = city.location.latitude
        && let Some(n) = serde_json::Number::from_f64(lat)
    {
        map.insert("latitude".into(), Value::Number(n));
    }
    if let Some(lon) = city.location.longitude
        && let Some(n) = serde_json::Number::from_f64(lon)
    {
        map.insert("longitude".into(), Value::Number(n));
    }

    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_geoip_no_database() {
        init(None);
        let result = lookup("8.8.8.8");
        assert!(result.is_err());
    }

    #[test]
    fn test_geoip_invalid_ip() {
        init(None);
        let result = lookup("not-an-ip");
        assert!(result.is_err());
    }
}
