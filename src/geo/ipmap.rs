//! RIPE IPmap (ipmap.ripe.net), an online source of router locations, used with
//! `--ipmap` only: each public address that answers goes to the RIPE NCC, which
//! may also schedule measurements of it from RIPE Atlas probes.
//!
//! Each answer carries a score. Those backed by latency measurements from
//! nearby probes scored 38 to 57 in practice; those backed only by reverse DNS,
//! crowdsourcing or city population scored 30 or less, and were wrong about as
//! often as right (Vienna and London for Arelion routers in Amsterdam and
//! Paris, at 16 and 18). So only city-level answers scoring 30 or more count,
//! and the light-speed check still applies to them. An address IPmap finds to be
//! anycast has no one place, so its answer doesn't count either.

use super::GeoFix;
use serde_json::Value;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::Duration;
use tracing::{debug, warn};

/// The credit shown wherever its locations are.
pub const ATTRIBUTION: &str = "RIPE IPmap, by the RIPE NCC";

/// Lowest score that counts
const MIN_SCORE: f64 = 30.0;

const API: &str = "https://ipmap-api.ripe.net/v1/locate";

/// Set once a failed lookup has been reported, so an unreachable IPmap warns once.
static WARNED: AtomicBool = AtomicBool::new(false);

/// One agent for every lookup, so they share connections.
static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(concat!(
            "mtr-ng/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/gedejong/mtr-ng)"
        ))
        .build()
        .into()
});

/// Where IPmap puts `addr`, if it is sure enough. Blocks until IPmap answers
/// or 10 seconds pass, so call it off the async workers.
pub fn locate(addr: IpAddr) -> Option<GeoFix> {
    let url = format!("{API}/{addr}/best");
    let body = AGENT
        .get(&url)
        .call()
        .and_then(|mut response| response.body_mut().read_to_string())
        .inspect_err(|e| {
            if WARNED.swap(true, Ordering::Relaxed) {
                debug!("RIPE IPmap on {addr}: {e}");
            } else {
                warn!("RIPE IPmap on {addr}: {e}");
            }
        })
        .ok()?;
    parse(&body)
}

/// The location in an answer from `/best`, if it names a city, scores at least
/// `MIN_SCORE` and isn't for an anycast address.
fn parse(body: &str) -> Option<GeoFix> {
    let answer: Value = serde_json::from_str(body).ok()?;
    if anycast(&answer) {
        return None;
    }
    let location = answer.get("location")?;
    let score = location.get("score")?.as_f64()?;
    if score < MIN_SCORE || location.get("type")?.as_str()? != "city" {
        return None;
    }
    let text = |key: &str| location.get(key)?.as_str().map(str::to_string);
    Some(GeoFix {
        lat: location.get("latitude")?.as_f64()?,
        lon: location.get("longitude")?.as_f64()?,
        accuracy_km: None,
        city: text("cityName"),
        country: text("countryCodeAlpha2"),
        from_name: None,
        from_ipmap: Some(score.round().clamp(0.0, 100.0) as u8),
    })
}

/// Whether an engine found the address to be anycast:
/// `metadata.service.contributions[addr].engines[].metadata.anycast`
fn anycast(answer: &Value) -> bool {
    let Some(Value::Object(addrs)) = answer.pointer("/metadata/service/contributions") else {
        return false;
    };
    addrs
        .values()
        .filter_map(|addr| addr.get("engines")?.as_array())
        .flatten()
        .any(|engine| engine.pointer("/metadata/anycast") == Some(&Value::Bool(true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real answer for 141.101.65.185, a Cloudflare router in
    /// Amsterdam, placed by latency measurements
    const MEASURED: &str = r#"{"location":{"id":"AMSTERDAM-NL-07-U173ZXCGGP7F8KXVWU3D","type":"city",
        "cityName":"Amsterdam","iataCode":"AMS","latitude":52.37403,"longitude":4.88969,
        "countryName":"Netherlands","countryCodeAlpha2":"NL","contributions":{"worlds":
        {"score":2},"latency":{"score":40}},"score":42.52323071310462},"alternatives":[]}"#;

    #[test]
    fn a_measured_city_counts() {
        let fix = parse(MEASURED).unwrap();
        assert_eq!(fix.place(), "Amsterdam, NL");
        assert_eq!((fix.lat, fix.lon), (52.37403, 4.88969));
        assert_eq!(fix.from_ipmap, Some(43));
        assert_eq!(fix.accuracy_km, None);
    }

    #[test]
    fn low_scores_countries_and_nothing_dont() {
        let guessed = MEASURED.replace("42.52323071310462", "18");
        assert!(
            parse(&guessed).is_none(),
            "crowdsourced and population only"
        );
        let country = MEASURED.replace(r#""type":"city""#, r#""type":"country""#);
        assert!(parse(&country).is_none(), "a country's middle is no place");
        assert!(parse(r#"{"location":null,"alternatives":[]}"#).is_none());
        let anycast = MEASURED.replace(
            r#""alternatives":[]"#,
            r#""alternatives":[],"metadata":{"service":{"contributions":{"1.1.1.1":
                {"engines":[{"engine":"latency","metadata":{"numClusters":38,"anycast":true}}]}}}}"#,
        );
        assert!(parse(&anycast).is_none(), "anycast: everywhere at once");
        let unicast = anycast.replace(r#""anycast":true"#, r#""anycast":null"#);
        assert!(parse(&unicast).is_some());
        assert!(parse("<html>Bad Gateway</html>").is_none());
    }
}
