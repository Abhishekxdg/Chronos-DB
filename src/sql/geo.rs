//! PostGIS's points: `geometry` (planar, any SRID) and `geography` (lon/lat on WGS84, SRID 4326).
//! Values travel as PostGIS prints them, hex EWKB; input is that, WKT or EWKT. Distances match
//! PostGIS: planar for geometry; for geography, on the WGS84 ellipsoid (the exact geodesic) or,
//! when asked, on its mean-radius sphere. Checked against PostGIS in tests/pgdiff.rs.
//! ponytail: points only (no lines or polygons), 2D only, no ST_Transform; the geodesic's
//! iteration doesn't converge for nearly antipodal points, which then use the sphere (off by up
//! to 0.3%); Karney's Newton step on the azimuth would fix that.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub srid: u32,
    pub x: f64,
    pub y: f64,
}

pub const WGS84: u32 = 4326;
const A: f64 = 6378137.0;
const F: f64 = 1.0 / 298.257223563;
/// PostGIS's sphere: the WGS84 ellipsoid's mean radius, (2a + b) / 3.
const R: f64 = (2.0 * A + A * (1.0 - F)) / 3.0;

fn only_points() -> String {
    "only points are supported here (not lines or polygons)".into()
}

impl Point {
    /// Hex EWKB, WKT or EWKT (`SRID=4326;POINT(1 2)`).
    pub fn parse(s: &str) -> Result<Point, String> {
        let s = s.trim();
        if s.len() >= 10 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Point::from_hex(s);
        }
        let (srid, wkt) = match s.split_once(';') {
            Some((h, rest)) if h.trim().to_ascii_uppercase().starts_with("SRID=") => {
                let n = h.trim()[5..].trim().parse::<u32>().map_err(|_| format!("invalid SRID in \"{s}\""))?;
                (n, rest.trim())
            }
            _ => (0, s),
        };
        let up = wkt.to_ascii_uppercase();
        let Some(rest) = up.strip_prefix("POINT") else {
            let word = up.split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("");
            return Err(
                if ["LINESTRING", "POLYGON", "MULTIPOINT", "MULTILINESTRING", "MULTIPOLYGON", "GEOMETRYCOLLECTION"]
                    .contains(&word)
                {
                    only_points()
                } else {
                    format!("parse error - invalid geometry: \"{s}\"")
                },
            );
        };
        let rest = rest.trim();
        if rest.starts_with('Z') || rest.starts_with('M') {
            return Err("3D and measured points aren't supported here".into());
        }
        if rest == "EMPTY" {
            return Ok(Point { srid, x: f64::NAN, y: f64::NAN });
        }
        let inner = rest
            .strip_prefix('(')
            .and_then(|r| r.strip_suffix(')'))
            .ok_or_else(|| format!("parse error - invalid geometry: \"{s}\""))?;
        let nums: Vec<f64> = inner
            .split_whitespace()
            .map(|t| t.parse::<f64>())
            .collect::<Result<_, _>>()
            .map_err(|_| format!("parse error - invalid geometry: \"{s}\""))?;
        match nums[..] {
            [x, y] => Ok(Point { srid, x, y }),
            [_, _, _, ..] => Err("3D and measured points aren't supported here".into()),
            _ => Err(format!("parse error - invalid geometry: \"{s}\"")),
        }
    }

    fn from_hex(s: &str) -> Result<Point, String> {
        if !s.len().is_multiple_of(2) {
            return Err(format!("Invalid hex string, length ({}) has to be a multiple of two!", s.len()));
        }
        let b: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let bad = || "invalid hex EWKB".to_string();
        let little = match b.first() {
            Some(1) => true,
            Some(0) => false,
            _ => return Err(bad()),
        };
        let u32_at = |i: usize| -> Result<u32, String> {
            let w: [u8; 4] = b.get(i..i + 4).ok_or_else(bad)?.try_into().unwrap();
            Ok(if little { u32::from_le_bytes(w) } else { u32::from_be_bytes(w) })
        };
        let f64_at = |i: usize| -> Result<f64, String> {
            let w: [u8; 8] = b.get(i..i + 8).ok_or_else(bad)?.try_into().unwrap();
            Ok(if little { f64::from_le_bytes(w) } else { f64::from_be_bytes(w) })
        };
        let ty = u32_at(1)?;
        if ty & 0xC000_0000 != 0 {
            return Err("3D and measured points aren't supported here".into());
        }
        if ty & 0x0FFF_FFFF != 1 {
            return Err(only_points());
        }
        let (srid, at) = if ty & 0x2000_0000 != 0 { (u32_at(5)?, 9) } else { (0, 5) };
        if b.len() != at + 16 {
            return Err(bad());
        }
        Ok(Point { srid, x: f64_at(at)?, y: f64_at(at + 8)? })
    }

    /// As PostGIS prints it: little-endian hex EWKB, uppercase.
    pub fn hex(&self) -> String {
        let mut b = vec![1u8];
        if self.srid != 0 {
            b.extend((1u32 | 0x2000_0000).to_le_bytes());
            b.extend(self.srid.to_le_bytes());
        } else {
            b.extend(1u32.to_le_bytes());
        }
        // PostGIS writes an empty point's NaNs as 0x7FF8...
        let nan = |v: f64| if v.is_nan() { f64::from_bits(0x7FF8_0000_0000_0000) } else { v };
        b.extend(nan(self.x).to_le_bytes());
        b.extend(nan(self.y).to_le_bytes());
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    /// As a geography: SRID 4326 (0 becomes it), coordinates brought into range as PostGIS does.
    pub fn geography(mut self) -> Result<Point, String> {
        match self.srid {
            0 | WGS84 => self.srid = WGS84,
            _ => return Err("Only lon/lat coordinate systems are supported in geography.".into()),
        }
        if self.x < -180.0 || self.x > 180.0 || self.y < -90.0 || self.y > 90.0 {
            self.x = lon_normalize(self.x);
            self.y = lat_normalize(self.y);
        }
        Ok(self)
    }

    fn empty(&self) -> bool {
        self.x.is_nan()
    }

    pub fn wkt(&self) -> String {
        if self.empty() {
            return "POINT EMPTY".into();
        }
        format!("POINT({} {})", num(self.x, 15), num(self.y, 15))
    }

    pub fn ewkt(&self) -> String {
        match self.srid {
            0 => self.wkt(),
            s => format!("SRID={s};{}", self.wkt()),
        }
    }

    pub fn geojson(&self, digits: usize) -> String {
        let crs = match self.srid {
            0 | WGS84 => String::new(),
            s => format!(r#""crs":{{"type":"name","properties":{{"name":"EPSG:{s}"}}}},"#),
        };
        let coords =
            if self.empty() { String::new() } else { format!("{},{}", num(self.x, digits), num(self.y, digits)) };
        format!(r#"{{"type":"Point",{crs}"coordinates":[{coords}]}}"#)
    }

    /// From a GeoJSON Point (SRID 4326, as PostGIS reads it).
    pub fn from_geojson(s: &str) -> Result<Point, String> {
        let j: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("invalid GeoJSON representation: {e}"))?;
        match j.get("type").and_then(|t| t.as_str()) {
            Some("Point") => {}
            Some(_) => return Err(only_points()),
            None => return Err("invalid GeoJSON representation".into()),
        }
        let c = j.get("coordinates").and_then(|c| c.as_array()).ok_or("invalid GeoJSON representation")?;
        match c.iter().map(|v| v.as_f64()).collect::<Option<Vec<_>>>().as_deref() {
            Some([x, y]) => Ok(Point { srid: WGS84, x: *x, y: *y }),
            Some([]) => Ok(Point { srid: WGS84, x: f64::NAN, y: f64::NAN }),
            _ => Err("invalid GeoJSON representation".into()),
        }
    }
}

/// A coordinate as PostGIS prints it: the shortest form, rounded to `digits` decimals; very big
/// numbers in exponent form.
fn num(v: f64, digits: usize) -> String {
    if v.abs() >= 1e15 {
        let e = format!("{v:e}");
        let (m, x) = e.split_once('e').unwrap();
        return format!(
            "{m}e{}{:02}",
            if x.starts_with('-') { '-' } else { '+' },
            x.trim_start_matches('-').parse::<u32>().unwrap_or(0)
        );
    }
    let short = v.to_string();
    let decimals = short.split_once('.').map_or(0, |(_, f)| f.len());
    let s = if decimals <= digits {
        short
    } else {
        let s = format!("{v:.digits$}");
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    };
    if s == "-0" { "0".into() } else { s }
}

fn lon_normalize(mut lon: f64) -> f64 {
    if lon > 360.0 {
        lon = remainder(lon, 360.0);
    }
    if lon < -360.0 {
        lon = remainder(lon, -360.0);
    }
    if lon > 180.0 {
        lon -= 360.0;
    }
    if lon < -180.0 {
        lon += 360.0;
    }
    if lon == -180.0 {
        return 180.0;
    }
    if lon == -360.0 {
        return 0.0;
    }
    lon
}

fn lat_normalize(mut lat: f64) -> f64 {
    if lat > 360.0 {
        lat = remainder(lat, 360.0);
    }
    if lat < -360.0 {
        lat = remainder(lat, -360.0);
    }
    if lat > 180.0 {
        lat = 180.0 - lat;
    }
    if lat < -180.0 {
        lat = -180.0 - lat;
    }
    if lat > 90.0 {
        lat = 180.0 - lat;
    }
    if lat < -90.0 {
        lat = -180.0 - lat;
    }
    lat
}

/// C's remainder(): x - n*y with n the nearest integer to x/y (ties to even).
fn remainder(x: f64, y: f64) -> f64 {
    let n = (x / y).round_ties_even();
    x - n * y
}

/// Planar distance; the SRIDs must match, as in PostGIS.
pub fn planar(a: &Point, b: &Point) -> Result<f64, String> {
    same_srid(a, b)?;
    Ok((a.x - b.x).hypot(b.y - a.y))
}

fn same_srid(a: &Point, b: &Point) -> Result<(), String> {
    if a.srid != b.srid {
        return Err(format!("Operation on mixed SRID geometries (Point, {}) != (Point, {})", a.srid, b.srid));
    }
    Ok(())
}

/// Great-circle distance in meters on PostGIS's sphere (the atan2 form, stable at any distance).
pub fn sphere(a: &Point, b: &Point) -> f64 {
    let rad = |d: f64| d * std::f64::consts::PI / 180.0;
    let (lat_s, lat_e) = (rad(a.y), rad(b.y));
    let d_lon = rad(b.x) - rad(a.x);
    let a1 = (lat_e.cos() * d_lon.sin()).powi(2);
    let a2 = (lat_s.cos() * lat_e.sin() - lat_s.sin() * lat_e.cos() * d_lon.cos()).powi(2);
    let y = (a1 + a2).sqrt();
    let x = lat_s.sin() * lat_e.sin() + lat_s.cos() * lat_e.cos() * d_lon.cos();
    y.atan2(x) * R
}

/// Gauss–Legendre nodes and weights on [-1, 1].
fn gauss(n: usize) -> Vec<(f64, f64)> {
    (1..=n)
        .map(|i| {
            let mut x = (std::f64::consts::PI * (i as f64 - 0.25) / (n as f64 + 0.5)).cos();
            loop {
                let (mut p0, mut p1) = (1.0, x);
                for k in 2..=n {
                    (p0, p1) = (p1, ((2 * k - 1) as f64 * x * p1 - (k - 1) as f64 * p0) / k as f64);
                }
                let dp = n as f64 * (x * p1 - p0) / (x * x - 1.0);
                let dx = p1 / dp;
                x -= dx;
                if dx.abs() < 1e-16 {
                    return (x, 2.0 / ((1.0 - x * x) * dp * dp));
                }
            }
        })
        .collect()
}

/// ∫ f from a to b, in pieces of at most π/8 with 16-point Gauss–Legendre: the geodesic
/// integrands are smooth, so this is exact to rounding.
fn integrate(a: f64, b: f64, f: impl Fn(f64) -> f64) -> f64 {
    static NODES: std::sync::LazyLock<Vec<(f64, f64)>> = std::sync::LazyLock::new(|| gauss(16));
    let pieces = ((b - a).abs() / (std::f64::consts::PI / 8.0)).ceil().max(1.0) as usize;
    let h = (b - a) / pieces as f64;
    (0..pieces)
        .map(|k| {
            let (lo, half) = (a + h * k as f64, h / 2.0);
            NODES.iter().map(|(x, w)| w * f(lo + half * (x + 1.0))).sum::<f64>() * half
        })
        .sum()
}

/// Geodesic distance in meters on the WGS84 ellipsoid: the exact integrals of Karney's
/// "Algorithms for geodesics" (eqs. 7 and 8), found by iterating on the longitude on the
/// auxiliary sphere and integrating numerically.
pub fn spheroid(p: &Point, q: &Point) -> f64 {
    let b = A * (1.0 - F);
    let ep2 = (A * A - b * b) / (b * b);
    let rad = |d: f64| d.to_radians();
    let beta = |lat: f64| ((1.0 - F) * rad(lat).tan()).atan();
    let (b1, b2) = (beta(p.y), beta(q.y));
    let (sb1, cb1, sb2, cb2) = (b1.sin(), b1.cos(), b2.sin(), b2.cos());
    let mut l = rad(q.x) - rad(p.x);
    l = (l + std::f64::consts::PI).rem_euclid(2.0 * std::f64::consts::PI) - std::f64::consts::PI;
    let mut omega = l;
    for _ in 0..200 {
        let (so, co) = (omega.sin(), omega.cos());
        let y = cb2 * so;
        let x = cb1 * sb2 - sb1 * cb2 * co;
        let sin_sigma = y.hypot(x);
        if sin_sigma == 0.0 {
            return 0.0; // the same point
        }
        let cos_sigma = sb1 * sb2 + cb1 * cb2 * co;
        let sigma12 = sin_sigma.atan2(cos_sigma);
        let sin_a0 = cb1 * cb2 * so / sin_sigma;
        let k2 = ep2 * (1.0 - sin_a0 * sin_a0);
        let alpha1 = y.atan2(x);
        let sigma1 = sb1.atan2(alpha1.cos() * cb1);
        let (s1, s2) = (sigma1, sigma1 + sigma12);
        let i_lambda = integrate(s1, s2, |s| (2.0 - F) / (1.0 + (1.0 - F) * (1.0 + k2 * s.sin().powi(2)).sqrt()));
        let next = l + F * sin_a0 * i_lambda;
        if (next - omega).abs() < 1e-15 {
            return b * integrate(s1, s2, |s| (1.0 + k2 * s.sin().powi(2)).sqrt());
        }
        omega = next;
    }
    sphere(p, q) // nearly antipodal: the iteration doesn't converge
}

/// Geography distance as PostGIS's ST_Distance returns it: rounded to 10 nanometers.
pub fn geography_distance(a: &Point, b: &Point, use_spheroid: bool) -> f64 {
    let d = if use_spheroid { spheroid(a, b) } else { sphere(a, b) };
    (d * 1e8).round() / 1e8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_and_measures_as_postgis() {
        let p = Point::parse("POINT(-122.4 37.8)").unwrap().geography().unwrap();
        assert_eq!(p.hex(), "0101000020E61000009A99999999995EC06666666666E64240");
        assert_eq!(Point::parse(&p.hex()).unwrap(), p);
        assert_eq!(Point::parse("point ( 1   2 )").unwrap().wkt(), "POINT(1 2)");
        let sf = Point { srid: WGS84, x: -122.4194, y: 37.7749 };
        let la = Point { srid: WGS84, x: -118.2437, y: 34.0522 };
        assert_eq!(geography_distance(&sf, &la, true), 559042.33650357);
        assert_eq!(geography_distance(&sf, &la, false), 559121.34684316);
        assert_eq!(Point::parse("POINT(-190 -95)").unwrap().geography().unwrap().wkt(), "POINT(170 -85)");
    }
}
