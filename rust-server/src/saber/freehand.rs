//! Port of the `perfect_freehand` Dart package (version 2.5.2, the one Saber pins) that turns
//! the raw pen input of a stroke into the outline polygon Saber fills on screen and in its own
//! PDF export. Keeping the same algorithm means the server-rendered PDF matches the app.

use std::f64::consts::PI;

const RATE_OF_PRESSURE_CHANGE: f64 = 0.275;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputPoint {
    pub x: f64,
    pub y: f64,
    pub pressure: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };
    pub const ONE: Vec2 = Vec2 { x: 1.0, y: 1.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn lerp(self, t: f64, other: Vec2) -> Vec2 {
        Vec2::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }

    fn rot_around(self, center: Vec2, radians: f64) -> Vec2 {
        let (s, c) = radians.sin_cos();
        let px = self.x - center.x;
        let py = self.y - center.y;
        Vec2::new(px * c - py * s + center.x, px * s + py * c + center.y)
    }

    fn distance_squared_to(self, other: Vec2) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        dx * dx + dy * dy
    }

    fn distance_to(self, other: Vec2) -> f64 {
        self.distance_squared_to(other).sqrt()
    }

    fn unit_vector_to(self, other: Vec2) -> Vec2 {
        let dx = other.x - self.x;
        let dy = other.y - self.y;
        let distance = (dx * dx + dy * dy).sqrt();
        Vec2::new(dx / distance, dy / distance)
    }

    fn dpr(self, other: Vec2) -> f64 {
        self.x * other.x + self.y * other.y
    }

    fn perpendicular(self) -> Vec2 {
        Vec2::new(self.y, -self.x)
    }

    fn unit(self) -> Vec2 {
        let length = (self.x * self.x + self.y * self.y).sqrt();
        if length == 0.0 {
            Vec2::ZERO
        } else {
            Vec2::new(self.x / length, self.y / length)
        }
    }

    fn project(self, direction: Vec2, distance: f64) -> Vec2 {
        Vec2::new(
            self.x + direction.x * distance,
            self.y + direction.y * distance,
        )
    }

    fn scale(self, factor: f64) -> Vec2 {
        Vec2::new(self.x * factor, self.y * factor)
    }

    fn add(self, other: Vec2) -> Vec2 {
        Vec2::new(self.x + other.x, self.y + other.y)
    }

    fn sub(self, other: Vec2) -> Vec2 {
        Vec2::new(self.x - other.x, self.y - other.y)
    }

    fn neg(self) -> Vec2 {
        Vec2::new(-self.x, -self.y)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndOptions {
    pub cap: bool,
    pub taper_enabled: bool,
    pub custom_taper: Option<f64>,
}

impl EndOptions {
    /// Mirrors `StrokeEndOptions` construction: a custom taper of `0` disables tapering and
    /// `-1` enables it without a custom length.
    pub fn from_json(custom_taper: Option<f64>, cap: bool) -> Self {
        match custom_taper {
            None => Self {
                cap,
                taper_enabled: false,
                custom_taper: None,
            },
            Some(0.0) => Self {
                cap,
                taper_enabled: false,
                custom_taper: None,
            },
            Some(-1.0) => Self {
                cap,
                taper_enabled: true,
                custom_taper: None,
            },
            Some(taper) => Self {
                cap,
                taper_enabled: true,
                custom_taper: Some(taper),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeOptions {
    pub size: f64,
    pub thinning: f64,
    pub smoothing: f64,
    pub streamline: f64,
    pub simulate_pressure: bool,
    pub start: EndOptions,
    pub end: EndOptions,
    pub is_complete: bool,
}

impl Default for StrokeOptions {
    /// Saber's `StrokeOptionsExtension.setDefaults()`.
    fn default() -> Self {
        Self {
            size: 10.0,
            thinning: 0.5,
            smoothing: 0.0,
            streamline: 0.5,
            simulate_pressure: true,
            start: EndOptions {
                cap: true,
                taper_enabled: false,
                custom_taper: None,
            },
            end: EndOptions {
                cap: true,
                taper_enabled: false,
                custom_taper: None,
            },
            is_complete: true,
        }
    }
}

struct StrokePoint {
    point: Vec2,
    pressure: f64,
    distance: f64,
    vector: Vec2,
    running_length: f64,
}

impl StrokePoint {
    fn simulate_pressure(&self, prev_pressure: f64, size: f64) -> f64 {
        let sp = (self.distance / size).min(1.0);
        let rp = (1.0 - sp).min(1.0);
        (prev_pressure + (rp - prev_pressure) * (sp * RATE_OF_PRESSURE_CHANGE)).min(1.0)
    }
}

fn ease_in_out(t: f64) -> f64 {
    t * (2.0 - t)
}

fn ease_out_cubic(t: f64) -> f64 {
    let t = t - 1.0;
    t * t * t + 1.0
}

fn stroke_radius(size: f64, thinning: f64, pressure: f64) -> f64 {
    size * (0.5 - thinning * (0.5 - pressure))
}

/// Returns the outline polygon of a stroke, in input coordinates.
pub fn get_stroke(points: &[InputPoint], options: &StrokeOptions) -> Vec<Vec2> {
    get_stroke_outline_points(&get_stroke_points(points, options), options)
}

fn get_stroke_points(points: &[InputPoint], options: &StrokeOptions) -> Vec<StrokePoint> {
    if points.is_empty() {
        return Vec::new();
    }
    let t = 0.15 + (1.0 - options.streamline) * 0.85;

    let mut pts: Vec<(Vec2, f64)> = points
        .iter()
        .map(|point| (Vec2::new(point.x, point.y), point.pressure.unwrap_or(0.5)))
        .collect();

    if pts.len() == 2 && pts[0] == pts[1] {
        pts.pop();
    }
    if pts.len() == 2 {
        let first = pts[0];
        let last = pts.pop().unwrap();
        for i in 1..5 {
            let f = i as f64 / 4.0;
            pts.push((first.0.lerp(f, last.0), first.1 + (last.1 - first.1) * f));
        }
    }
    if pts.len() == 1 {
        let first = pts[0];
        pts.push((Vec2::new(first.0.x + 1.0, first.0.y + 1.0), first.1));
    }

    let mut stroke_points = vec![StrokePoint {
        point: pts[0].0,
        pressure: pts[0].1,
        vector: Vec2::ONE,
        distance: 0.0,
        running_length: 0.0,
    }];
    let mut has_reached_minimum_length = false;
    let mut running_length = 0.0;
    let max = pts.len() - 1;

    for (i, (input, pressure)) in pts.iter().enumerate() {
        let prev = stroke_points.last().unwrap();
        let point = if options.is_complete && i == max {
            *input
        } else {
            prev.point.lerp(t, *input)
        };
        if point == prev.point {
            continue;
        }
        let distance = point.distance_to(prev.point);
        running_length += distance;
        if i < max && !has_reached_minimum_length {
            if running_length < options.size {
                continue;
            }
            has_reached_minimum_length = true;
        }
        let vector = point.unit_vector_to(prev.point);
        stroke_points.push(StrokePoint {
            point,
            pressure: *pressure,
            vector,
            distance,
            running_length,
        });
    }

    if stroke_points.len() > 1 {
        stroke_points[0].vector = stroke_points[1].vector;
    } else {
        stroke_points[0].vector = Vec2::ZERO;
    }
    stroke_points
}

fn get_stroke_outline_points(points: &[StrokePoint], options: &StrokeOptions) -> Vec<Vec2> {
    if points.is_empty() || options.size <= 0.0 {
        return Vec::new();
    }
    let size = options.size;
    let total_length = points.last().unwrap().running_length;
    let taper_start = if options.start.taper_enabled {
        options.start.custom_taper.unwrap_or(size.max(total_length))
    } else {
        0.0
    };
    let taper_end = if options.end.taper_enabled {
        options.end.custom_taper.unwrap_or(size.max(total_length))
    } else {
        0.0
    };
    let min_distance = (size * options.smoothing).powi(2);

    let mut left_points: Vec<Vec2> = Vec::new();
    let mut right_points: Vec<Vec2> = Vec::new();

    let mut prev_pressure = {
        let mut smoothed = points[0].pressure;
        let count = (points.len() - 1).min(10);
        for curr in &points[..count] {
            let pressure = if options.simulate_pressure {
                curr.simulate_pressure(smoothed, size)
            } else {
                curr.pressure
            };
            smoothed = (smoothed + pressure) / 2.0;
        }
        smoothed
    };

    let mut radius = stroke_radius(size, options.thinning, points.last().unwrap().pressure);
    let mut first_radius: Option<f64> = None;
    let mut prev_vector = points[0].vector;
    let mut pl = points[0].point;
    let mut pr = pl;
    let mut tl;
    let mut tr;
    let mut is_prev_point_sharp_corner = false;

    for i in 0..points.len() {
        let point = points[i].point;
        let vector = points[i].vector;
        let running_length = points[i].running_length;

        if i < points.len() - 1
            && options.is_complete
            && !is_prev_point_sharp_corner
            && total_length - running_length < size / 2.0
        {
            continue;
        }

        if options.thinning != 0.0 {
            let pressure = if options.simulate_pressure {
                let pressure = points[i].simulate_pressure(prev_pressure, size);
                prev_pressure = pressure;
                pressure
            } else {
                points[i].pressure
            };
            radius = stroke_radius(size, options.thinning, pressure);
        } else {
            radius = size / 2.0;
        }
        if first_radius.is_none() {
            first_radius = Some(radius);
        }

        let ts = if running_length < taper_start {
            ease_in_out(running_length / taper_start)
        } else {
            1.0
        };
        let te = if total_length - running_length < taper_end {
            ease_out_cubic((total_length - running_length) / taper_end)
        } else {
            1.0
        };
        radius = (radius * ts.min(te)).max(0.01);

        let next_vector = if i < points.len() - 1 {
            points[i + 1].vector
        } else {
            vector
        };
        let next_dpr = if i < points.len() - 1 {
            vector.dpr(next_vector)
        } else {
            1.0
        };
        let prev_dpr = vector.dpr(prev_vector);

        let max_dpr_for_sharp_corner = size / 128.0;
        let is_point_sharp_corner =
            prev_dpr < max_dpr_for_sharp_corner && !is_prev_point_sharp_corner;
        let is_next_point_sharp_corner = next_dpr < max_dpr_for_sharp_corner;

        if is_point_sharp_corner || is_next_point_sharp_corner {
            let prev_offset = prev_vector.perpendicular().scale(radius);
            let step = 1.0 / 13.0;
            let mut t = 0.0;
            while t <= 1.0 {
                tl = point.sub(prev_offset).rot_around(point, PI * t);
                left_points.push(tl);
                tr = point.add(prev_offset).rot_around(point, PI * -t);
                right_points.push(tr);
                t += step;
            }
            let next_offset = next_vector.perpendicular().scale(radius);
            tl = point.add(next_offset).rot_around(point, -PI);
            tr = point.sub(next_offset).rot_around(point, PI);
            left_points.push(tl);
            right_points.push(tr);
            pl = tr;
            pr = tl;
            if is_next_point_sharp_corner {
                is_prev_point_sharp_corner = true;
            }
            continue;
        }

        is_prev_point_sharp_corner = false;

        if i == points.len() - 1 {
            let offset = vector.perpendicular().scale(radius);
            left_points.push(point.sub(offset));
            right_points.push(point.add(offset));
            continue;
        }

        let offset = next_vector
            .lerp(next_dpr, vector)
            .perpendicular()
            .scale(radius);
        tl = point.sub(offset);
        if i <= 1 || pl.distance_squared_to(tl) > min_distance {
            left_points.push(tl);
            pl = tl;
        }
        tr = point.add(offset);
        if i <= 1 || pr.distance_squared_to(tr) > min_distance {
            right_points.push(tr);
            pr = tr;
        }
        prev_vector = vector;
    }

    let first_point = points[0].point;
    let last_point = if points.len() > 1 {
        points.last().unwrap().point
    } else {
        first_point.add(points[0].vector)
    };

    let mut start_cap: Vec<Vec2> = Vec::new();
    let mut end_cap: Vec<Vec2> = Vec::new();

    if points.len() == 1 {
        if !(taper_start > 0.0 || taper_end > 0.0) || options.is_complete {
            let start = first_point.project(
                first_point.sub(last_point).perpendicular().unit(),
                -first_radius.unwrap_or(radius),
            );
            let mut dot_points = Vec::new();
            let step = 1.0 / 13.0;
            let mut t = step;
            while t <= 1.0 {
                dot_points.push(start.rot_around(first_point, PI * 2.0 * t));
                t += step;
            }
            return dot_points;
        }
    } else if taper_start > 0.0 || (taper_end > 0.0 && points.len() == 1) {
        // Tapered start: no cap.
    } else if options.start.cap {
        if let Some(first_right) = right_points.first().copied() {
            let step = 1.0 / 13.0;
            let mut t = step;
            while t <= 1.0 {
                start_cap.push(first_right.rot_around(first_point, PI * t));
                t += step;
            }
        }
    } else if let (Some(first_left), Some(first_right)) =
        (left_points.first().copied(), right_points.first().copied())
    {
        let corners_vector = first_left.sub(first_right);
        let offset_a = corners_vector.scale(0.5);
        let offset_b = corners_vector.scale(0.51);
        start_cap.push(first_point.sub(offset_a));
        start_cap.push(first_point.sub(offset_b));
        start_cap.push(first_point.add(offset_b));
        start_cap.push(first_point.add(offset_a));
    }

    let direction = points.last().unwrap().vector.neg().perpendicular();
    if taper_end > 0.0 || (taper_start > 0.0 && points.len() == 1) {
        end_cap.push(last_point);
    } else if options.end.cap {
        let start = last_point.project(direction, radius);
        let step = 1.0 / 29.0;
        let mut t = step;
        while t <= 1.0 {
            end_cap.push(start.rot_around(last_point, PI * 3.0 * t));
            t += step;
        }
    } else {
        end_cap.push(last_point.add(direction.scale(radius)));
        end_cap.push(last_point.add(direction.scale(radius * 0.99)));
        end_cap.push(last_point.sub(direction.scale(radius * 0.99)));
        end_cap.push(last_point.sub(direction.scale(radius)));
    }

    let mut outline = left_points;
    outline.extend(end_cap);
    outline.extend(right_points.into_iter().rev());
    outline.extend(start_cap);
    outline
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize) -> Vec<InputPoint> {
        (0..n)
            .map(|i| InputPoint {
                x: 100.0 + i as f64 * 20.0,
                y: 100.0,
                pressure: Some(0.5),
            })
            .collect()
    }

    #[test]
    fn straight_line_outline_surrounds_the_input() {
        let outline = get_stroke(&line(12), &StrokeOptions::default());
        assert!(outline.len() > 20, "got {} points", outline.len());
        let min_x = outline.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
        let max_x = outline
            .iter()
            .map(|p| p.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = outline.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
        let max_y = outline
            .iter()
            .map(|p| p.y)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(min_x < 100.0 && max_x > 320.0, "x range {min_x}..{max_x}");
        assert!(min_y < 100.0 && max_y > 100.0, "y range {min_y}..{max_y}");
        assert!(
            max_y - min_y <= 12.0,
            "line is too thick: {}",
            max_y - min_y
        );
        assert!(outline.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
    }

    #[test]
    fn single_point_becomes_a_small_blob() {
        // Like the Dart original, a lone point gets a second point one unit away and is drawn
        // as a tiny capped stroke rather than being dropped.
        let outline = get_stroke(&line(1), &StrokeOptions::default());
        assert!(outline.len() >= 13, "got {} points", outline.len());
        let max_x = outline
            .iter()
            .map(|p| p.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_x = outline.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
        assert!(max_x - min_x <= 14.0, "blob too wide: {}", max_x - min_x);
        assert!(outline.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
    }

    #[test]
    fn empty_input_has_no_outline() {
        assert!(get_stroke(&[], &StrokeOptions::default()).is_empty());
    }

    #[test]
    fn taper_options_parse_like_dart() {
        assert!(!EndOptions::from_json(Some(0.0), true).taper_enabled);
        let enabled = EndOptions::from_json(Some(-1.0), true);
        assert!(enabled.taper_enabled && enabled.custom_taper.is_none());
        assert_eq!(
            EndOptions::from_json(Some(30.0), false).custom_taper,
            Some(30.0)
        );
    }
}
