//! Best-effort IP geolocation. Prefers platform-provided country headers
//! (Cloudflare / Vercel); falls back to a free ip-api.com lookup (cached).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub struct GeoInfo {
    pub country: String,
    pub region: String,
    pub city: String,
}

impl GeoInfo {
    pub fn unknown() -> Self {
        GeoInfo { country: "Unknown".into(), region: String::new(), city: String::new() }
    }
    pub fn just_country(c: &str) -> Self {
        GeoInfo { country: c.to_string(), region: String::new(), city: String::new() }
    }
}

#[derive(Deserialize)]
struct IpApiResp {
    status: String,
    country: Option<String>,
    #[serde(rename = "regionName")]
    region_name: Option<String>,
    city: Option<String>,
}

/// Resolve a country/region for an IP via ip-api.com (HTTP, no key, free tier).
/// Returns Unknown on any failure or for private/local addresses.
pub async fn lookup(ip: &str) -> GeoInfo {
    if ip.is_empty() || is_private(ip) {
        return GeoInfo::just_country("Local");
    }
    let url = format!("http://ip-api.com/json/{ip}?fields=status,country,regionName,city");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1800))
        .build()
    {
        Ok(c) => c,
        Err(_) => return GeoInfo::unknown(),
    };
    match client.get(&url).send().await {
        Ok(resp) => match resp.json::<IpApiResp>().await {
            Ok(j) if j.status == "success" => GeoInfo {
                country: j.country.unwrap_or_else(|| "Unknown".into()),
                region: j.region_name.unwrap_or_default(),
                city: j.city.unwrap_or_default(),
            },
            _ => GeoInfo::unknown(),
        },
        Err(_) => GeoInfo::unknown(),
    }
}

fn is_private(ip: &str) -> bool {
    ip == "127.0.0.1"
        || ip == "::1"
        || ip == "0.0.0.0"
        || ip.starts_with("10.")
        || ip.starts_with("192.168.")
        || ip.starts_with("169.254.")
        || ip.starts_with("172.16.")
        || ip.starts_with("172.17.")
        || ip.starts_with("172.18.")
        || ip.starts_with("172.19.")
        || ip.starts_with("172.2")
        || ip.starts_with("172.30.")
        || ip.starts_with("172.31.")
        || ip.starts_with("fc")
        || ip.starts_with("fd")
}
