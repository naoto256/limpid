//! geoip: MaxMind GeoIP database lookup.
//!
//! Used as an expression function in DSL:
//!   `workspace.geo = geoip(workspace.src_ip)`
//!
//! Returns an object: `{ country, city, latitude, longitude }`

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Result;
use maxminddb::{Reader, geoip2};

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};

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

/// Look up an IP address and return an arena-backed object with
/// geo-location data. Fields that are unavailable are omitted (not
/// nulled) so DSL conditionals (`if workspace.geo.city == ...`) stay
/// unambiguous.
pub fn lookup<'bump>(arena: &EventArena<'bump>, ip_str: &str) -> Result<Value<'bump>> {
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

    let mut builder = ObjectBuilder::with_capacity(arena, 4);

    if let Some(iso) = city.country.iso_code {
        builder.push_str("country", Value::String(arena.alloc_str(iso)));
    }
    if let Some(name) = city.city.names.english {
        builder.push_str("city", Value::String(arena.alloc_str(name)));
    }
    if let Some(lat) = city.location.latitude
        && lat.is_finite()
    {
        builder.push_str("latitude", Value::Float(lat));
    }
    if let Some(lon) = city.location.longitude
        && lon.is_finite()
    {
        builder.push_str("longitude", Value::Float(lon));
    }

    Ok(builder.finish())
}
