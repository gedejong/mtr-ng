//! Writes the map's coastlines, `src/ui/coastline.bin`, from Natural Earth's 1:50m
//! coastlines (public domain):
//!
//! ```sh
//! curl -fLO https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_50m_coastline.geojson
//! cargo run --example coastline ne_50m_coastline.geojson > src/ui/coastline.bin
//! ```
//!
//! Per line a little-endian u16 point count, then that many (lon, lat) pairs as
//! little-endian i16 hundredths of a degree, about a kilometre: finer than a
//! terminal cell at any zoom the map uses.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{self, Write};

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: coastline ne_50m_coastline.geojson")?;
    let geojson: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let features = geojson["features"].as_array().context("no features")?;
    let mut out = Vec::new();
    for feature in features {
        let geometry = &feature["geometry"];
        let lines = match geometry["type"].as_str() {
            Some("LineString") => vec![&geometry["coordinates"]],
            Some("MultiLineString") => geometry["coordinates"]
                .as_array()
                .context("no coordinates")?
                .iter()
                .collect(),
            other => bail!("unexpected geometry {other:?}"),
        };
        for line in lines {
            let mut points: Vec<(i16, i16)> = Vec::new();
            for point in line.as_array().context("no points")? {
                let lon = point[0].as_f64().context("no longitude")?;
                let lat = point[1].as_f64().context("no latitude")?;
                let point = ((lon * 100.0).round() as i16, (lat * 100.0).round() as i16);
                if points.last() != Some(&point) {
                    points.push(point);
                }
            }
            if points.len() < 2 {
                continue;
            }
            out.extend(u16::try_from(points.len())?.to_le_bytes());
            for (lon, lat) in points {
                out.extend(lon.to_le_bytes());
                out.extend(lat.to_le_bytes());
            }
        }
    }
    io::stdout().write_all(&out)?;
    Ok(())
}
