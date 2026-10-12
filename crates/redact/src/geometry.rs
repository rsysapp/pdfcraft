//! Embed build (R6b-54, N1): cutting paths exactly along redaction regions.
//!
//! A path meeting a region loses every subpath whose geometry meets it; what such a subpath has
//! outside the regions is rebuilt from its outline, flattened (curves to lines within
//! [`FLATNESS`] of a point in user space), so that nothing of its geometry is left under a
//! region:
//! - for filling and clipping, by exact polygon clipping against each region's complement. The
//!   complement of a rectangle is cut into four convex parts (left, right, and below and above
//!   it between its sides); a polygon clipped against each part (Sutherland–Hodgman, which keeps
//!   the winding number at every point inside the part) gives pieces whose winding numbers add
//!   up to the original's at every point outside the region and to nothing inside it, so the
//!   nonzero and even-odd rules both fill exactly what they filled outside the region. Pieces
//!   share their cuts exactly (each crossing is computed once per edge and side), so painted in
//!   one path they leave no seam;
//! - for stroking, by cutting the outline's segments where they enter and leave a region: what
//!   lies outside stays, in pieces.
//!
//! Every point the pieces hold lies outside the regions or on their edges.

use pdfcraft_content::Matrix;

/// How far (user space) a flattened curve may lie from the curve.
pub(crate) const FLATNESS: f64 = 0.01;
/// The most points one path's pieces may hold.
pub(crate) const MAX_POINTS: usize = 200_000;

pub(crate) type Point = (f64, f64);

/// One segment of a subpath, in user space.
#[derive(Clone, Copy, Debug)]
enum Segment {
    Line(Point),
    Curve(Point, Point, Point),
}

/// A subpath: its start, its segments, whether `h` closed it, and the operators that built it.
#[derive(Clone, Debug)]
pub(crate) struct Subpath {
    start: Point,
    segments: Vec<Segment>,
    pub closed: bool,
    /// The box of every point and control point, in user space.
    pub bounds: [f64; 4],
    /// The operators that built it, as written.
    pub ops: Vec<pdfcraft_content::Op>,
}

fn grow(bounds: &mut [f64; 4], (x, y): Point) {
    *bounds = [bounds[0].min(x), bounds[1].min(y), bounds[2].max(x), bounds[3].max(y)];
}

/// The subpaths path construction operators `ops` build, mapped through `ctm`. `None` when they
/// are not a path every viewer builds alike (a segment with no current point, an operand
/// missing).
pub(crate) fn subpaths(ops: &[pdfcraft_content::Op], ctm: &Matrix) -> Option<Vec<Subpath>> {
    let mut out: Vec<Subpath> = Vec::new();
    let mut current: Option<Subpath> = None;
    let mut point: Option<Point> = None;
    let empty = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for op in ops {
        let o = op.op.as_slice();
        match o {
            b"m" => {
                let [x, y] = op.nums::<2>()?;
                out.extend(current.take());
                let p = ctm.apply(x, y);
                let mut bounds = empty;
                grow(&mut bounds, p);
                current = Some(Subpath { start: p, segments: Vec::new(), closed: false, bounds, ops: vec![op.clone()] });
                point = Some(p);
            }
            b"re" => {
                let [x, y, w, h] = op.nums::<4>()?;
                out.extend(current.take());
                let corners = [(x, y), (x + w, y), (x + w, y + h), (x, y + h)].map(|(u, v)| ctm.apply(u, v));
                let mut bounds = empty;
                for corner in corners {
                    grow(&mut bounds, corner);
                }
                out.push(Subpath {
                    start: corners[0],
                    segments: corners[1..].iter().map(|c| Segment::Line(*c)).collect(),
                    closed: true,
                    bounds,
                    ops: vec![op.clone()],
                });
                // After `re` the current point is its corner (x, y), starting no subpath of its own.
                point = Some(corners[0]);
            }
            b"l" | b"c" | b"v" | b"y" => {
                let from = point?;
                let sub = current.as_mut()?;
                let segment = match o {
                    b"l" => {
                        let [x, y] = op.nums::<2>()?;
                        Segment::Line(ctm.apply(x, y))
                    }
                    b"c" => {
                        let [x1, y1, x2, y2, x3, y3] = op.nums::<6>()?;
                        Segment::Curve(ctm.apply(x1, y1), ctm.apply(x2, y2), ctm.apply(x3, y3))
                    }
                    b"v" => {
                        let [x2, y2, x3, y3] = op.nums::<4>()?;
                        Segment::Curve(from, ctm.apply(x2, y2), ctm.apply(x3, y3))
                    }
                    _ => {
                        let [x1, y1, x3, y3] = op.nums::<4>()?;
                        let end = ctm.apply(x3, y3);
                        Segment::Curve(ctm.apply(x1, y1), end, end)
                    }
                };
                let end = match segment {
                    Segment::Line(p) => {
                        grow(&mut sub.bounds, p);
                        p
                    }
                    Segment::Curve(a, b, c) => {
                        grow(&mut sub.bounds, a);
                        grow(&mut sub.bounds, b);
                        grow(&mut sub.bounds, c);
                        c
                    }
                };
                sub.segments.push(segment);
                sub.ops.push(op.clone());
                point = Some(end);
            }
            b"h" => {
                if let Some(sub) = current.as_mut() {
                    sub.closed = true;
                    sub.ops.push(op.clone());
                    point = Some(sub.start);
                    // A segment after `h` starts a new subpath at the start point.
                    let mut bounds = empty;
                    grow(&mut bounds, sub.start);
                    let start = sub.start;
                    out.extend(current.take());
                    current = Some(Subpath { start, segments: Vec::new(), closed: false, bounds, ops: Vec::new() });
                }
            }
            // `W`, `W*`: the clip flag, kept by the caller.
            _ => {}
        }
    }
    out.extend(current.take());
    // A subpath begun only by `h` (no segment, no operator of its own) is nothing.
    out.retain(|sub| !sub.ops.is_empty());
    Some(out)
}

impl Subpath {
    /// Its operators as written, starting with a move to its start where they do not (a
    /// subpath begun by a segment after `h` starts where the previous one did, which may be
    /// written elsewhere once pieces are cut out of a path).
    pub(crate) fn ops_standing_alone(&self, back: &Matrix) -> Vec<pdfcraft_content::Op> {
        let mut out = Vec::with_capacity(self.ops.len() + 1);
        if !self.ops.first().is_some_and(|op| op.is("m") || op.is("re")) {
            let (u, v) = back.apply(self.start.0, self.start.1);
            out.push(pdfcraft_content::Op::new("m", vec![pdfcraft_content::num(u), pdfcraft_content::num(v)]));
        }
        out.extend(self.ops.iter().cloned());
        out
    }
}

/// Flatten a curve from `p0` through controls `p1`, `p2` to `p3` into `out` (`p0` excluded),
/// within [`FLATNESS`]; `budget` counts the points made.
fn flatten_curve(p0: Point, p1: Point, p2: Point, p3: Point, depth: usize, out: &mut Vec<Point>, budget: &mut usize) -> Option<()> {
    let distance = |p: Point| {
        let (dx, dy) = (p3.0 - p0.0, p3.1 - p0.1);
        let length = (dx * dx + dy * dy).sqrt();
        if length < 1e-12 { ((p.0 - p0.0).powi(2) + (p.1 - p0.1).powi(2)).sqrt() } else { ((p.0 - p0.0) * dy - (p.1 - p0.1) * dx).abs() / length }
    };
    if depth >= 24 || (distance(p1) <= FLATNESS && distance(p2) <= FLATNESS) {
        *budget = budget.checked_sub(1)?;
        out.push(p3);
        return Some(());
    }
    let mid = |a: Point, b: Point| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
    let (p01, p12, p23) = (mid(p0, p1), mid(p1, p2), mid(p2, p3));
    let (p012, p123) = (mid(p01, p12), mid(p12, p23));
    let centre = mid(p012, p123);
    flatten_curve(p0, p01, p012, centre, depth + 1, out, budget)?;
    flatten_curve(centre, p123, p23, p3, depth + 1, out, budget)
}

/// The subpath's outline as points in user space (its start first; a closed subpath's closing
/// segment is not repeated). `None` past `budget` points.
pub(crate) fn outline(sub: &Subpath, budget: &mut usize) -> Option<Vec<Point>> {
    let mut points = vec![sub.start];
    let mut at = sub.start;
    for segment in &sub.segments {
        match *segment {
            Segment::Line(p) => {
                *budget = budget.checked_sub(1)?;
                points.push(p);
                at = p;
            }
            Segment::Curve(a, b, c) => {
                flatten_curve(at, a, b, c, 0, &mut points, budget)?;
                at = c;
            }
        }
    }
    Some(points)
}

/// One side of a half-plane: `x <= at` (`Left`), `x >= at`, `y <= at`, `y >= at`.
#[derive(Clone, Copy)]
enum Side {
    Left(f64),
    Right(f64),
    Below(f64),
    Above(f64),
}

impl Side {
    fn inside(self, (x, y): Point) -> bool {
        match self {
            Side::Left(at) => x <= at,
            Side::Right(at) => x >= at,
            Side::Below(at) => y <= at,
            Side::Above(at) => y >= at,
        }
    }

    /// Where segment `a`–`b` crosses the side's line, computed the same whichever way the
    /// segment runs (so neighbouring pieces share their cut exactly).
    fn crossing(self, a: Point, b: Point) -> Point {
        let (a, b) = if (a.0, a.1) <= (b.0, b.1) { (a, b) } else { (b, a) };
        match self {
            Side::Left(at) | Side::Right(at) => {
                let t = (at - a.0) / (b.0 - a.0);
                (at, a.1 + t * (b.1 - a.1))
            }
            Side::Below(at) | Side::Above(at) => {
                let t = (at - a.1) / (b.1 - a.1);
                (a.0 + t * (b.0 - a.0), at)
            }
        }
    }
}

/// Sutherland–Hodgman: `polygon` (closed) clipped to one half-plane.
fn clip_half(polygon: &[Point], side: Side) -> Vec<Point> {
    let mut out = Vec::with_capacity(polygon.len() + 4);
    for (i, &current) in polygon.iter().enumerate() {
        let previous = polygon[(i + polygon.len() - 1) % polygon.len()];
        match (side.inside(previous), side.inside(current)) {
            (true, true) => out.push(current),
            (true, false) => out.push(side.crossing(previous, current)),
            (false, true) => {
                out.push(side.crossing(previous, current));
                out.push(current);
            }
            (false, false) => {}
        }
    }
    out
}

fn polygon_bounds(polygon: &[Point]) -> [f64; 4] {
    let mut bounds = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for p in polygon {
        grow(&mut bounds, *p);
    }
    bounds
}

/// The pieces of a filled (or clipping) polygon outside every region (see the module
/// documentation). `None` past [`MAX_POINTS`].
pub(crate) fn clip_fill(polygon: Vec<Point>, regions: &[[f64; 4]]) -> Option<Vec<Vec<Point>>> {
    let mut pieces = vec![polygon];
    let mut points = 0usize;
    for r in regions {
        let mut next = Vec::with_capacity(pieces.len());
        for piece in pieces {
            if !pdfcraft_content::overlaps(*r, polygon_bounds(&piece), 0.0) {
                points = points.checked_add(piece.len())?;
                next.push(piece);
                continue;
            }
            // The region's complement, in four convex parts.
            let parts: [&[Side]; 4] = [
                &[Side::Left(r[0])],
                &[Side::Right(r[2])],
                &[Side::Right(r[0]), Side::Left(r[2]), Side::Below(r[1])],
                &[Side::Right(r[0]), Side::Left(r[2]), Side::Above(r[3])],
            ];
            for part in parts {
                let mut clipped = piece.clone();
                for side in part {
                    if clipped.is_empty() {
                        break;
                    }
                    clipped = clip_half(&clipped, *side);
                }
                if clipped.len() >= 3 {
                    points = points.checked_add(clipped.len())?;
                    next.push(clipped);
                }
            }
            if points > MAX_POINTS {
                return None;
            }
        }
        pieces = next;
    }
    Some(pieces)
}

/// The parts of segment `a`–`b` inside region `r` (open), as parameter ranges of the segment.
fn inside_range(a: Point, b: Point, r: &[f64; 4]) -> Option<(f64, f64)> {
    // Liang–Barsky against the open rectangle.
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut low, mut high) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, a.0 - r[0]), (dx, r[2] - a.0), (-dy, a.1 - r[1]), (dy, r[3] - a.1)] {
        if p.abs() < 1e-15 {
            if q <= 0.0 {
                return None;
            }
            continue;
        }
        let t = q / p;
        if p < 0.0 {
            low = low.max(t);
        } else {
            high = high.min(t);
        }
    }
    (low < high).then_some((low, high))
}

/// The pieces of a stroked polyline (`closed`: its closing segment too) outside every region,
/// each an open polyline of two points or more. `None` past [`MAX_POINTS`].
pub(crate) fn clip_stroke(points: &[Point], closed: bool, regions: &[[f64; 4]]) -> Option<Vec<Vec<Point>>> {
    let mut pieces: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    let mut total = 0usize;
    let count = if closed { points.len() } else { points.len().saturating_sub(1) };
    for i in 0..count {
        let a = points[i];
        let b = points[(i + 1) % points.len()];
        let mut inside: Vec<(f64, f64)> = regions.iter().filter_map(|r| inside_range(a, b, r)).collect();
        inside.sort_by(|x, y| x.0.total_cmp(&y.0));
        let at = |t: f64| (a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1));
        let mut t = 0.0;
        for (low, high) in inside {
            if low > t {
                if current.is_empty() {
                    current.push(at(t));
                }
                current.push(at(low));
            }
            if current.len() >= 2 {
                total = total.checked_add(current.len())?;
                pieces.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            t = t.max(high);
        }
        if t < 1.0 {
            if current.is_empty() {
                current.push(at(t));
            }
            current.push(b);
        }
        if total > MAX_POINTS {
            return None;
        }
    }
    if current.len() >= 2 {
        pieces.push(current);
    }
    Some(pieces)
}

/// Whether a stroke's subpath, closed, meets no region but by the pieces' joins: the first
/// piece continues the last (a closed outline cut once).
pub(crate) fn join_ends(pieces: &mut Vec<Vec<Point>>, closed: bool) {
    if !closed || pieces.len() < 2 {
        return;
    }
    let (Some(first), Some(last)) = (pieces.first(), pieces.last()) else { return };
    if first.first() == last.last() {
        let first = pieces.remove(0);
        if let Some(last) = pieces.last_mut() {
            last.extend(first.into_iter().skip(1));
        }
    }
}

/// `points` (user space) written as a subpath in the path's own space (`back`: the inverse of the
/// matrix the path is drawn with): `m`, then `l` to each point, then `h` when `close`.
pub(crate) fn write(points: &[Point], back: &Matrix, close: bool, out: &mut Vec<pdfcraft_content::Op>) {
    for (i, (x, y)) in points.iter().enumerate() {
        let (u, v) = back.apply(*x, *y);
        out.push(pdfcraft_content::Op::new(if i == 0 { "m" } else { "l" }, vec![pdfcraft_content::num(u), pdfcraft_content::num(v)]));
    }
    if close && !points.is_empty() {
        out.push(pdfcraft_content::Op::new("h", vec![]));
    }
}

/// Whether a path's box `b` meets region `r`: a positive overlap once the box is given a hair
/// of extent (a straight line's box has no height or no width).
pub(crate) fn meets(r: [f64; 4], b: [f64; 4]) -> bool {
    const HAIR: f64 = 1e-6;
    pdfcraft_content::overlaps(r, [b[0] - HAIR, b[1] - HAIR, b[2] + HAIR, b[3] + HAIR], 0.0)
}

/// Whether point `p` lies inside region `r` by more than `slack` on every side.
pub(crate) fn strictly_inside(p: Point, r: &[f64; 4], slack: f64) -> bool {
    p.0 > r[0] + slack && p.0 < r[2] - slack && p.1 > r[1] + slack && p.1 < r[3] - slack
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The winding number of `polygon` around `p`.
    fn winding(polygon: &[Point], p: Point) -> i32 {
        let mut w = 0;
        for i in 0..polygon.len() {
            let (a, b) = (polygon[i], polygon[(i + 1) % polygon.len()]);
            if a.1 <= p.1 {
                if b.1 > p.1 && (b.0 - a.0) * (p.1 - a.1) - (p.0 - a.0) * (b.1 - a.1) > 0.0 {
                    w += 1;
                }
            } else if b.1 <= p.1 && (b.0 - a.0) * (p.1 - a.1) - (p.0 - a.0) * (b.1 - a.1) < 0.0 {
                w -= 1;
            }
        }
        w
    }

    #[test]
    fn filled_pieces_keep_the_winding_outside_the_regions_and_nothing_inside() {
        // A self-intersecting star, a ring wound twice: every sample point's winding number is
        // the original's outside the regions and zero inside them.
        let star: Vec<Point> = (0..5)
            .map(|i| {
                let angle = std::f64::consts::PI / 2.0 + f64::from(i) * 4.0 * std::f64::consts::PI / 5.0;
                (50.0 + 40.0 * angle.cos(), 50.0 + 40.0 * angle.sin())
            })
            .collect();
        let regions = [[30.0, 30.0, 60.0, 55.0], [55.0, 50.0, 95.0, 70.0]];
        let pieces = clip_fill(star.clone(), &regions).unwrap();
        for i in 0..100 {
            for j in 0..100 {
                let p = (f64::from(i) + 0.37, f64::from(j) + 0.61);
                let total: i32 = pieces.iter().map(|piece| winding(piece, p)).sum();
                let under = regions.iter().any(|r| p.0 > r[0] && p.0 < r[2] && p.1 > r[1] && p.1 < r[3]);
                assert_eq!(total, if under { 0 } else { winding(&star, p) }, "{p:?}");
            }
        }
        // No point of a piece lies inside a region.
        assert!(pieces.iter().flatten().all(|p| !regions.iter().any(|r| strictly_inside(*p, r, 1e-9))));
    }

    #[test]
    fn stroked_pieces_are_what_lies_outside() {
        let line = [(0.0, 10.0), (100.0, 10.0), (100.0, 50.0)];
        let pieces = clip_stroke(&line, false, &[[20.0, 0.0, 30.0, 20.0], [90.0, 20.0, 110.0, 30.0]]).unwrap();
        assert_eq!(
            pieces,
            vec![vec![(0.0, 10.0), (20.0, 10.0)], vec![(30.0, 10.0), (100.0, 10.0), (100.0, 20.0)], vec![(100.0, 30.0), (100.0, 50.0)]]
        );
    }
}
