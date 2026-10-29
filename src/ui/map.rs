//! The path on a world map: every located hop a numbered spot, consecutive ones
//! joined along great circles. A location the hop's RTT rules out stays off the
//! map: the path didn't go there, and drawing it would also stretch the zoom.

use super::render::track_rows;
use super::state::UiState;
use super::visualization::{coast_color, ColorSupport};
use crate::geo::{self, GeoFix};
use crate::topology::ranges;
use crate::utils::text::pack;
use crate::{HopStats, MtrSession};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::Line,
    widgets::{
        canvas::{Canvas, Line as Segment},
        Paragraph, Wrap,
    },
    Frame,
};
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Smallest area the map is drawn in.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

/// Degrees between the points of a drawn great circle.
const ARC_STEP: f64 = 2.0;

/// Smallest span shown, in degrees, so a path within one region isn't a dot.
const MIN_SPAN: f64 = 6.0;

/// Natural Earth's 1:50m coastlines (public domain), as `examples/coastline.rs`
/// writes them. Drawn as lines rather than ratatui's `Map` points, which fall
/// apart into loose dots once the map zooms in on a region.
static COASTLINE: &[u8] = include_bytes!("coastline.bin");

/// One coastline as (lon, lat) points, with its bounding box.
struct Coast {
    lon: [f64; 2],
    lat: [f64; 2],
    points: Vec<(f64, f64)>,
}

impl Coast {
    fn overlaps(&self, x: [f64; 2], y: [f64; 2]) -> bool {
        self.lon[0] <= x[1] && x[0] <= self.lon[1] && self.lat[0] <= y[1] && y[0] <= self.lat[1]
    }
}

fn coastlines() -> &'static [Coast] {
    static COASTS: OnceLock<Vec<Coast>> = OnceLock::new();
    COASTS.get_or_init(|| {
        let degrees = |lo, hi| f64::from(i16::from_le_bytes([lo, hi])) / 100.0;
        let mut coasts = Vec::new();
        let mut bytes = COASTLINE;
        while let Some((count, rest)) = bytes.split_first_chunk::<2>() {
            let (data, rest) = rest.split_at(usize::from(u16::from_le_bytes(*count)) * 4);
            let points: Vec<(f64, f64)> = data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&[a, b, c, d]| (degrees(a, b), degrees(c, d)))
                .collect();
            let span = |of: fn(&(f64, f64)) -> f64| {
                let values = points.iter().map(of);
                [
                    values.clone().fold(f64::MAX, f64::min),
                    values.fold(f64::MIN, f64::max),
                ]
            };
            coasts.push(Coast {
                lon: span(|p| p.0),
                lat: span(|p| p.1),
                points,
            });
            bytes = rest;
        }
        coasts
    })
}

/// The inhabited world, for paths across the date line.
const WORLD: ([f64; 2], [f64; 2]) = ([-180.0, 180.0], [-60.0, 85.0]);

/// Hops that fall in one cell of the map.
struct Spot {
    cell: (u16, u16),
    hops: BTreeSet<u16>,
    selected: bool,
}

impl Spot {
    fn label(&self) -> String {
        ranges(&self.hops)
    }
}

/// Map coordinates to the cells ratatui prints canvas labels in, and back.
struct Grid {
    x: [f64; 2],
    y: [f64; 2],
    cols: u16,
    rows: u16,
}

impl Grid {
    fn steps(&self) -> (f64, f64) {
        let (cols, rows) = (f64::from(self.cols - 1), f64::from(self.rows - 1));
        (
            (self.x[1] - self.x[0]) / cols,
            (self.y[1] - self.y[0]) / rows,
        )
    }

    /// The cell of (latitude, longitude), as ratatui computes it for a label.
    fn cell(&self, (lat, lon): (f64, f64)) -> (u16, u16) {
        let (dx, dy) = self.steps();
        let col = ((lon - self.x[0]) / dx).clamp(0.0, f64::from(self.cols - 1)) as u16;
        let row = ((self.y[1] - lat) / dy).clamp(0.0, f64::from(self.rows - 1)) as u16;
        (col, row)
    }

    /// A point that ratatui prints in `cell`: (x, y), that is (longitude, latitude).
    fn point(&self, (col, row): (u16, u16)) -> (f64, f64) {
        let (dx, dy) = self.steps();
        let x = (self.x[0] + (f64::from(col) + 0.01) * dx).min(self.x[1]);
        let y = (self.y[1] - (f64::from(row) + 0.01) * dy).max(self.y[0]);
        (x, y)
    }
}

pub fn render_map(
    f: &mut Frame,
    area: Rect,
    session: &MtrSession,
    hops: &[HopStats],
    ui: &mut UiState,
) {
    let shown: Vec<&HopStats> = hops.iter().filter(|hop| hop.sent > 0).collect();
    track_rows(ui, shown.iter().map(|hop| hop.hop).collect());
    // No table clamps the selection here
    if ui.table.selected().is_some_and(|row| row >= shown.len()) {
        ui.table.select(shown.len().checked_sub(1));
    }
    let message = |f: &mut Frame, lines: Vec<Line<'static>>| {
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    };
    let Some(credit) = session.geo_attribution() else {
        return message(f, no_database());
    };
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return message(
            f,
            vec![Line::from("Too small for the map. m returns to the table.")],
        );
    }
    let origin = session.origin();
    let (located, ruled_out): (Vec<(&HopStats, &GeoFix)>, Vec<_>) = shown
        .iter()
        .filter_map(|&hop| Some((hop, hop.geo.as_ref()?)))
        .partition(|(hop, _)| !hop.assess(origin.as_ref()).is_some_and(|a| a.impossible));
    let ruled_out: BTreeSet<u16> = ruled_out
        .iter()
        .map(|(hop, _)| u16::from(hop.hop))
        .collect();
    if located.is_empty() {
        let mut text = "No hop located yet. Private addresses, and those the database doesn't \
                        know, stay off the map."
            .to_string();
        if !ruled_out.is_empty() {
            text.push_str(&format!(
                " So do locations their RTT rules out: hop {}.",
                ranges(&ruled_out)
            ));
        }
        return message(f, vec![Line::from(text)]);
    }

    let arcs: Vec<Vec<(f64, f64)>> = located
        .windows(2)
        .map(|w| geo::great_circle((w[0].1.lat, w[0].1.lon), (w[1].1.lat, w[1].1.lon), ARC_STEP))
        .collect();
    let given_origin = origin.filter(|o| o.hop.is_none());
    let mut points: Vec<(f64, f64)> = arcs.iter().flatten().copied().collect();
    points.extend(located.iter().map(|(_, fix)| (fix.lat, fix.lon)));
    points.extend(given_origin.map(|o| (o.lat, o.lon)));

    let unplaced: BTreeSet<u16> = shown
        .iter()
        .filter(|hop| hop.geo.is_none())
        .map(|hop| u16::from(hop.hop))
        .collect();
    let mut key = vec!["●n hop n".to_string()];
    if given_origin.is_some() {
        key.push("◆ --origin".to_string());
    }
    if !unplaced.is_empty() {
        key.push(format!("not placed: {}", ranges(&unplaced)));
    }
    if !ruled_out.is_empty() {
        key.push(format!("ruled out by RTT: {}", ranges(&ruled_out)));
    }
    if let Some(detour) = session.inflation() {
        key.push(format!(
            "path {:.0} km, {:.1}× the great circle",
            detour.path_km,
            detour.ratio()
        ));
    }
    let mut legend: Vec<Line> = pack(&key, " · ", usize::from(area.width))
        .into_iter()
        .map(Line::from)
        .collect();
    legend.push(Line::styled(
        credit.to_string(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    let [map_area, legend_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(legend.len() as u16)]).areas(area);
    let crosses_date_line = arcs
        .iter()
        .any(|arc| geo::split_at_antimeridian(arc).len() > 1);
    let (x, y) = match crosses_date_line {
        true => WORLD,
        false => bounds(&points, map_area.width, map_area.height),
    };
    let grid = Grid {
        x,
        y,
        cols: map_area.width,
        rows: map_area.height,
    };
    let spots = spots(&located, ui.selected_hop(), &grid);
    let labels = place_labels(&spots, &grid);
    let tint = |color| match ui.colors {
        ColorSupport::None => Color::Reset,
        _ => color,
    };
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds(x)
        .y_bounds(y)
        .paint(|ctx| {
            let coast = coast_color(ui.colors);
            for line in coastlines().iter().filter(|line| line.overlaps(x, y)) {
                for pair in line.points.windows(2) {
                    let (from, to) = (pair[0], pair[1]);
                    ctx.draw(&Segment::new(from.0, from.1, to.0, to.1, coast));
                }
            }
            ctx.layer();
            let lines = arcs.iter().flat_map(|arc| geo::split_at_antimeridian(arc));
            for line in lines {
                for pair in line.windows(2) {
                    let (from, to) = (pair[0], pair[1]);
                    ctx.draw(&Segment::new(
                        from.1,
                        from.0,
                        to.1,
                        to.0,
                        tint(Color::Yellow),
                    ));
                }
            }
            ctx.layer();
            if let Some(o) = given_origin {
                ctx.print(
                    o.lon,
                    o.lat,
                    Line::styled("◆", Style::default().fg(tint(Color::Cyan))),
                );
            }
            for (spot, label) in spots.iter().zip(&labels) {
                let mut style = Style::default().add_modifier(Modifier::BOLD);
                if spot.selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let (x, y) = grid.point(spot.cell);
                ctx.print(x, y, Line::styled("●", style));
                let (x, y) = grid.point(*label);
                ctx.print(x, y, Line::styled(spot.label(), style));
            }
        });
    f.render_widget(canvas, map_area);
    f.render_widget(Paragraph::new(legend), legend_area);
}

/// Located hops grouped by the cell they fall in, in order of first appearance.
fn spots(located: &[(&HopStats, &GeoFix)], selected: Option<u8>, grid: &Grid) -> Vec<Spot> {
    let mut spots: Vec<Spot> = Vec::new();
    for &(hop, fix) in located {
        let cell = grid.cell((fix.lat, fix.lon));
        let spot = match spots.iter().position(|spot| spot.cell == cell) {
            Some(i) => &mut spots[i],
            None => {
                spots.push(Spot {
                    cell,
                    hops: BTreeSet::new(),
                    selected: false,
                });
                spots.last_mut().unwrap()
            }
        };
        spot.hops.insert(u16::from(hop.hop));
        spot.selected |= selected == Some(hop.hop);
    }
    spots
}

/// Where each spot's hop numbers go: right of its dot, on the nearest row where
/// they cover no other dot or label, else left of it, else on the dot's row anyway.
fn place_labels(spots: &[Spot], grid: &Grid) -> Vec<(u16, u16)> {
    let mut taken: BTreeSet<(u16, u16)> = spots.iter().map(|spot| spot.cell).collect();
    let mut placed = Vec::new();
    for spot in spots {
        let width = spot.label().chars().count() as u16;
        let (col, row) = spot.cell;
        let free = |taken: &BTreeSet<(u16, u16)>, start: u16, row: u16| {
            start + width <= grid.cols && (start..start + width).all(|c| !taken.contains(&(c, row)))
        };
        let rows = [0i32, -1, 1, -2, 2]
            .into_iter()
            .filter_map(|d| u16::try_from(i32::from(row) + d).ok())
            .filter(|&r| r < grid.rows);
        let starts = [Some(col + 1), col.checked_sub(width)];
        let found = rows
            .flat_map(|r| starts.into_iter().flatten().map(move |start| (start, r)))
            .find(|&(start, r)| free(&taken, start, r));
        let (start, r) = found.unwrap_or((col + 1, row));
        taken.extend((start..start + width).map(|c| (c, r)));
        placed.push((start, r));
    }
    placed
}

/// A step between two points that jumps across the date line.
/// Longitude and latitude ranges that show `points` (latitude, longitude) with a
/// margin on a canvas `cols` × `rows` cells, stretched so a kilometre looks as
/// long across as up. A cell is about twice as tall as it is wide.
fn bounds(points: &[(f64, f64)], cols: u16, rows: u16) -> ([f64; 2], [f64; 2]) {
    let fold = |pick: fn(&(f64, f64)) -> f64| {
        let values = points.iter().map(pick);
        let lo = values.clone().fold(f64::INFINITY, f64::min);
        (lo, values.fold(f64::NEG_INFINITY, f64::max))
    };
    let (lat, lon) = (fold(|p| p.0), fold(|p| p.1));
    let span = |(lo, hi): (f64, f64)| ((hi - lo) * 1.2).max(MIN_SPAN);
    let (mut lat_span, mut lon_span) = (span(lat), span(lon));
    let (lat_mid, lon_mid) = ((lat.0 + lat.1) / 2.0, (lon.0 + lon.1) / 2.0);

    // Kilometres per degree of longitude shrink with the cosine of the latitude
    let squeeze = lat_mid.to_radians().cos().max(0.2);
    let screen = f64::from(cols) / (f64::from(rows) * 2.0);
    if lon_span * squeeze / lat_span < screen {
        lon_span = screen * lat_span / squeeze;
    } else {
        lat_span = lon_span * squeeze / screen;
    }
    (
        clamp_range(lon_mid, lon_span, 180.0),
        clamp_range(lat_mid, lat_span, 90.0),
    )
}

/// `mid ± span/2`, shifted to stay within `±limit`, and at most the whole range.
fn clamp_range(mid: f64, span: f64, limit: f64) -> [f64; 2] {
    let half = (span / 2.0).min(limit);
    let mid = mid.clamp(-limit + half, limit - half);
    [mid - half, mid + half]
}

fn no_database() -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    vec![
        Line::styled("No geolocation database, so no map.", bold),
        Line::from(""),
        Line::from("DB-IP's free City Lite database (CC BY 4.0, about 130 MB) can be fetched with"),
        Line::from(""),
        Line::from(geo::DOWNLOAD),
        Line::from(""),
        Line::from(
            "mtr-ng finds it there when it starts. --geoip-mmdb PATH takes another file, \
             such as MaxMind's GeoLite2 City. Lookups stay on this machine.",
        ),
        Line::from(""),
        Line::from("m returns to the table."),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_keep_the_aspect_and_a_minimum_span() {
        let amsterdam = [(52.37, 4.89)];
        let ([x0, x1], [y0, y1]) = bounds(&amsterdam, 80, 20);
        assert!(x0 < 4.89 && 4.89 < x1 && y0 < 52.37 && 52.37 < y1);
        assert!(y1 - y0 >= MIN_SPAN - 1e-9);
        let km_ratio = (x1 - x0) * 52.37f64.to_radians().cos() / (y1 - y0);
        assert!(
            (km_ratio - 2.0).abs() < 0.01,
            "80 cells across, 40 half-cells up: {km_ratio}"
        );

        let ([x0, x1], [y0, y1]) = bounds(&[(70.0, -170.0), (-50.0, 170.0)], 80, 20);
        assert!(
            x0 >= -180.0 && x1 <= 180.0 && y0 >= -90.0 && y1 <= 90.0,
            "clamped"
        );
    }

    fn spot(cell: (u16, u16), hops: &[u16]) -> Spot {
        Spot {
            cell,
            hops: hops.iter().copied().collect(),
            selected: false,
        }
    }

    #[test]
    fn grid_cells_round_trip_through_points() {
        let grid = Grid {
            x: [-10.0, 30.0],
            y: [40.0, 60.0],
            cols: 41,
            rows: 21,
        };
        for cell in [(0, 0), (40, 20), (17, 3)] {
            let (x, y) = grid.point(cell);
            assert_eq!(grid.cell((y, x)), cell);
        }
        assert_eq!(grid.cell((52.0, 4.0)), (14, 8));
    }

    #[test]
    fn labels_avoid_dots_and_each_other() {
        let grid = Grid {
            x: [0.0, 10.0],
            y: [0.0, 10.0],
            cols: 20,
            rows: 10,
        };
        let spots = [
            spot((5, 5), &[2, 3, 4]),
            spot((6, 5), &[7, 9]),
            spot((19, 0), &[8]),
        ];
        let labels = place_labels(&spots, &grid);
        assert_eq!(
            labels[0],
            (2, 5),
            "the next dot is in the way: left of its own"
        );
        assert_eq!(labels[1], (7, 5), "\"7, 9\" fits right of its dot");
        assert_eq!(labels[2], (18, 0), "no room at the right edge");

        let crowded = [spot((5, 5), &[1]), spot((6, 5), &[2]), spot((4, 5), &[3])];
        let labels = place_labels(&crowded, &grid);
        assert_eq!(
            labels,
            [(6, 4), (7, 5), (3, 5)],
            "both sides taken: a row up"
        );
    }

    #[test]
    fn coastlines_decode_into_lines_on_the_globe() {
        let coasts = coastlines();
        assert_eq!(coasts.len(), 1429);
        assert!(coasts.iter().all(|c| c.points.len() >= 2
            && c.lon[0] >= -180.0
            && c.lon[1] <= 180.0
            && c.lat[0] >= -90.0
            && c.lat[1] <= 90.0));
        let ijmuiden = (4.6, 52.46);
        let nearest = coasts
            .iter()
            .flat_map(|c| &c.points)
            .map(|p| (p.0 - ijmuiden.0).hypot(p.1 - ijmuiden.1))
            .fold(f64::MAX, f64::min);
        assert!(nearest < 0.2, "the Dutch coast passes IJmuiden: {nearest}°");
        let europe = coasts
            .iter()
            .filter(|c| c.overlaps([0.0, 10.0], [48.0, 55.0]))
            .count();
        assert!((1..100).contains(&europe), "culling keeps a few: {europe}");
    }

    #[test]
    fn clamped_ranges_shift_rather_than_shrink() {
        assert_eq!(clamp_range(170.0, 40.0, 180.0), [140.0, 180.0]);
        assert_eq!(clamp_range(0.0, 500.0, 180.0), [-180.0, 180.0]);
    }
}
