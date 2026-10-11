//! Encoding shading patterns for easy sampling.

use crate::color::{AlphaColor, ColorComponents, ColorSpace};
use crate::interpret::state::ActiveTransferFunction;
use crate::pattern::ShadingPattern;
use crate::shading::{ShadingFunction, ShadingType, Triangle};
use kurbo::{Affine, Point};
use rustc_hash::FxHashMap;
use smallvec::{ToSmallVec, smallvec};

/// A shading pattern that was encoded so it can be sampled.
#[derive(Debug)]
pub struct EncodedShadingPattern {
    /// The base transform of the shading pattern.
    pub base_transform: Affine,
    pub(crate) color_space: ColorSpace,
    pub(crate) background_color: AlphaColor,
    pub(crate) shading_type: EncodedShadingType,
    pub(crate) opacity: f32,
    pub(crate) transfer_function: Option<ActiveTransferFunction>,
}

impl EncodedShadingPattern {
    /// Sample the shading at the given position.
    #[inline]
    pub fn sample(&self, pos: Point) -> [f32; 4] {
        self.shading_type
            .eval(pos, self.background_color, &self.color_space)
            .map(|v| {
                let mut components = v.components();
                components[3] *= self.opacity;

                if let Some(tf) = &self.transfer_function {
                    return tf.apply(&AlphaColor::new(components)).components();
                }

                components
            })
            .unwrap_or([0.0, 0.0, 0.0, 0.0])
    }
}

/// Embed build: the most triangles a mesh shading may be sampled from (a Coons or tensor patch
/// is 722 triangles): more is refused before they are built.
const MAX_MESH_TRIANGLES: u64 = 500_000;

impl ShadingPattern {
    /// Encode the shading pattern.
    pub fn encode(&self) -> EncodedShadingPattern {
        // Never `None` without an area: nothing is charged.
        self.encode_with(None).unwrap_or_else(|| self.dummy())
    }

    /// Embed build: encode the shading for sampling inside `area` (device space) only, charging
    /// the work and allocations of meshes (`crate::spend_work`) before doing them. `None` when
    /// the work budget refuses them.
    pub fn encode_within(&self, area: kurbo::Rect) -> Option<EncodedShadingPattern> {
        self.encode_with(Some(area))
    }

    /// Embed build: the cost of one sample, in function evaluations' operators (see
    /// `Function::cost`); 1 for a mesh without a function.
    pub fn evaluation_cost(&self) -> u64 {
        match self.shading.shading_type.as_ref() {
            ShadingType::FunctionBased { function, .. } | ShadingType::RadialAxial { function, .. } => function.cost(),
            ShadingType::TriangleMesh { function, .. }
            | ShadingType::CoonsPatchMesh { function, .. }
            | ShadingType::TensorProductPatchMesh { function, .. } => function.as_ref().map_or(1, ShadingFunction::cost),
            ShadingType::Dummy => 1,
        }
    }

    fn dummy(&self) -> EncodedShadingPattern {
        EncodedShadingPattern {
            color_space: self.shading.color_space.clone(),
            background_color: AlphaColor::TRANSPARENT,
            shading_type: EncodedShadingType::Dummy,
            base_transform: Affine::IDENTITY,
            opacity: self.opacity,
            transfer_function: None,
        }
    }

    fn encode_with(&self, area: Option<kurbo::Rect>) -> Option<EncodedShadingPattern> {
        let base_transform;

        let shading_type = match self.shading.shading_type.as_ref() {
            ShadingType::FunctionBased {
                domain,
                matrix,
                function,
            } => {
                base_transform = (self.matrix * *matrix).inverse();
                encode_function_shading(domain, function)
            }
            ShadingType::RadialAxial {
                coords,
                domain,
                function,
                extend,
                axial,
            } => {
                let (encoded, initial_transform) =
                    encode_axial_shading(*coords, *domain, function, *extend, *axial);

                base_transform = initial_transform * self.matrix.inverse();

                encoded
            }
            ShadingType::TriangleMesh {
                triangles,
                function,
            } => {
                let full_transform = self.matrix;
                let samples = sample_triangles(triangles, full_transform, area)?;

                base_transform = Affine::IDENTITY;

                EncodedShadingType::Sampled {
                    samples,
                    function: function.clone(),
                }
            }
            ShadingType::CoonsPatchMesh { patches, function } => {
                // Embed build: the triangles a patch mesh becomes are counted before being built.
                if area.is_some() && !mesh_size_ok(patches.len().saturating_mul(PATCH_TRIANGLES)) {
                    return None;
                }
                let mut triangles = vec![];
                for patch in patches {
                    patch.to_triangles(&mut triangles);
                }

                let full_transform = self.matrix;
                let samples = sample_triangles(&triangles, full_transform, area)?;

                base_transform = Affine::IDENTITY;

                EncodedShadingType::Sampled {
                    samples,
                    function: function.clone(),
                }
            }
            ShadingType::TensorProductPatchMesh { patches, function } => {
                // Embed build: the triangles a patch mesh becomes are counted before being built.
                if area.is_some() && !mesh_size_ok(patches.len().saturating_mul(PATCH_TRIANGLES)) {
                    return None;
                }
                let mut triangles = vec![];
                for patch in patches {
                    patch.to_triangles(&mut triangles);
                }

                let full_transform = self.matrix;
                let samples = sample_triangles(&triangles, full_transform, area)?;

                base_transform = Affine::IDENTITY;

                EncodedShadingType::Sampled {
                    samples,
                    function: function.clone(),
                }
            }
            ShadingType::Dummy => {
                base_transform = Affine::IDENTITY;

                EncodedShadingType::Dummy
            }
        };

        let color_space = self.shading.color_space.clone();

        let background_color = self
            .shading
            .background
            .as_ref()
            .map(|b| color_space.to_rgba(b, 1.0, false))
            .unwrap_or(AlphaColor::TRANSPARENT);

        Some(EncodedShadingPattern {
            color_space,
            background_color,
            shading_type,
            base_transform,
            opacity: self.opacity,
            transfer_function: self.transfer_function.clone(),
        })
    }
}

/// Embed build: the triangles one patch becomes (`generate_patch_triangles`: a 20 × 20 grid).
const PATCH_TRIANGLES: usize = 19 * 19 * 2;

/// Embed build: whether a mesh of `triangles` may be built and sampled.
fn mesh_size_ok(triangles: usize) -> bool {
    let triangles = u64::try_from(triangles).unwrap_or(u64::MAX);
    triangles <= MAX_MESH_TRIANGLES && crate::spend_work(triangles)
}

fn encode_axial_shading(
    coords: [f32; 6],
    domain: [f32; 2],
    function: &ShadingFunction,
    extend: [bool; 2],
    is_axial: bool,
) -> (EncodedShadingType, Affine) {
    let initial_transform;

    let params = if is_axial {
        let [x_0, y_0, x_1, y_1, _, _] = coords;

        initial_transform = ts_from_line_to_line(
            Point::new(x_0 as f64, y_0 as f64),
            Point::new(x_1 as f64, y_1 as f64),
            Point::ZERO,
            Point::new(1.0, 0.0),
        );

        RadialAxialParams::Axial
    } else {
        let [x_0, y_0, r0, x_1, y_1, r_1] = coords;

        initial_transform = Affine::translate((-x_0 as f64, -y_0 as f64));
        let new_x1 = x_1 - x_0;
        let new_y1 = y_1 - y_0;

        let p1 = Point::new(new_x1 as f64, new_y1 as f64);
        let r = Point::new(r0 as f64, r_1 as f64);

        RadialAxialParams::Radial { p1, r }
    };

    (
        EncodedShadingType::RadialAxial {
            function: function.clone(),
            params,
            domain,
            extend,
        },
        initial_transform,
    )
}

/// Embed build: `area` (device space) limits the sampling to the pixels a texture of that area
/// reads, so a triangle far larger than the page is not walked; the triangles, the pixels walked
/// and the map's size are charged before anything is sampled. `None`: refused by the budget.
fn sample_triangles(
    triangles: &[Triangle],
    transform: Affine,
    area: Option<kurbo::Rect>,
) -> Option<FxHashMap<(u16, u16), ColorComponents>> {
    let mut map = FxHashMap::default();
    // Pixel centres up to one pixel past the area are read.
    let area = area.map(|a| a.inflate(1.0, 1.0));
    let placed = |t: &Triangle| {
        let p0 = transform * t.p0.point;
        let p1 = transform * t.p1.point;
        let p2 = transform * t.p2.point;
        kurbo::Rect::from_points(p0, p1).union_pt(p2)
    };
    if let Some(area) = area {
        if !mesh_size_ok(triangles.len()) {
            return None;
        }
        let mut pixels = 0f64;
        for t in triangles {
            let covered = placed(t).intersect(area);
            pixels += if covered.is_finite() { covered.area().max(0.0) } else { 0.0 };
        }
        // About 14 ns a pixel walked; a map entry takes up to about 128 bytes as the map grows
        // (measured), one per pixel at most.
        let entries = pixels.min(area.area().max(0.0));
        if !crate::spend_allocation((entries * 128.0).min(u64::MAX as f64) as u64) || !crate::spend_work((pixels / 32.0).min(u64::MAX as f64) as u64) {
            return None;
        }
    }

    for t in triangles {
        let t = {
            let p0 = transform * t.p0.point;
            let p1 = transform * t.p1.point;
            let p2 = transform * t.p2.point;

            let mut v0 = t.p0.clone();
            v0.point = p0;
            let mut v1 = t.p1.clone();
            v1.point = p1;
            let mut v2 = t.p2.clone();
            v2.point = p2;

            Triangle::new(v0, v1, v2)
        };

        let bbox = match area {
            Some(area) => t.bounding_box().intersect(area),
            None => t.bounding_box(),
        };
        if !(bbox.width() > 0.0 && bbox.height() > 0.0) {
            continue;
        }

        for y in (bbox.y0.floor() as u16)..(bbox.y1.ceil() as u16) {
            for x in (bbox.x0.floor() as u16)..(bbox.x1.ceil() as u16) {
                let point = Point::new(x as f64, y as f64);
                if t.contains_point(point) {
                    map.insert((x, y), t.interpolate(point));
                }
            }
        }
    }

    Some(map)
}

fn encode_function_shading(domain: &[f32; 4], function: &ShadingFunction) -> EncodedShadingType {
    let domain = kurbo::Rect::new(
        domain[0] as f64,
        domain[2] as f64,
        domain[1] as f64,
        domain[3] as f64,
    );

    EncodedShadingType::FunctionBased {
        domain,
        function: function.clone(),
    }
}

#[derive(Debug)]
pub(crate) enum RadialAxialParams {
    Axial,
    Radial { p1: Point, r: Point },
}

#[derive(Debug)]
pub(crate) enum EncodedShadingType {
    FunctionBased {
        domain: kurbo::Rect,
        function: ShadingFunction,
    },
    RadialAxial {
        function: ShadingFunction,
        params: RadialAxialParams,
        domain: [f32; 2],
        extend: [bool; 2],
    },
    Sampled {
        samples: FxHashMap<(u16, u16), ColorComponents>,
        function: Option<ShadingFunction>,
    },
    Dummy,
}

impl EncodedShadingType {
    pub(crate) fn eval(
        &self,
        pos: Point,
        bg_color: AlphaColor,
        color_space: &ColorSpace,
    ) -> Option<AlphaColor> {
        match self {
            Self::FunctionBased { domain, function } => {
                if !domain.contains(pos) {
                    Some(bg_color)
                } else {
                    let out = function.eval(&smallvec![pos.x as f32, pos.y as f32])?;
                    // TODO: Clamp out-of-range values.
                    Some(color_space.to_rgba(&out, 1.0, false))
                }
            }
            Self::RadialAxial {
                function,
                params,
                domain,
                extend,
            } => {
                let (t0, t1) = (domain[0], domain[1]);

                let mut t = match params {
                    RadialAxialParams::Axial => pos.x as f32,
                    RadialAxialParams::Radial { p1, r } => {
                        radial_pos(&pos, p1, *r, extend[0], extend[1]).unwrap_or(f32::MIN)
                    }
                };

                if t == f32::MIN {
                    return Some(bg_color);
                }

                if t < 0.0 {
                    if extend[0] {
                        t = 0.0;
                    } else {
                        return Some(bg_color);
                    }
                } else if t > 1.0 {
                    if extend[1] {
                        t = 1.0;
                    } else {
                        return Some(bg_color);
                    }
                }

                let t = t0 + (t1 - t0) * t;

                let val = function.eval(&smallvec![t])?;

                Some(color_space.to_rgba(&val, 1.0, false))
            }
            Self::Sampled { samples, function } => {
                let sample_point = (pos.x as u16, pos.y as u16);

                if let Some(color) = samples.get(&sample_point) {
                    if let Some(function) = function {
                        let val = function.eval(&color.to_smallvec())?;
                        Some(color_space.to_rgba(&val, 1.0, false))
                    } else {
                        Some(color_space.to_rgba(color, 1.0, false))
                    }
                } else {
                    Some(bg_color)
                }
            }
            Self::Dummy => Some(AlphaColor::TRANSPARENT),
        }
    }
}

fn ts_from_line_to_line(src1: Point, src2: Point, dst1: Point, dst2: Point) -> Affine {
    let unit_to_line1 = unit_to_line(src1, src2);
    let line1_to_unit = unit_to_line1.inverse();
    let unit_to_line2 = unit_to_line(dst1, dst2);

    unit_to_line2 * line1_to_unit
}

fn unit_to_line(p0: Point, p1: Point) -> Affine {
    Affine::new([
        p1.y - p0.y,
        p0.x - p1.x,
        p1.x - p0.x,
        p1.y - p0.y,
        p0.x,
        p0.y,
    ])
}

fn radial_pos(
    pos: &Point,
    p1: &Point,
    r: Point,
    min_extend: bool,
    max_extend: bool,
) -> Option<f32> {
    let r0 = r.x as f32;
    let dx = p1.x as f32;
    let dy = p1.y as f32;
    let dr = r.y as f32 - r0;

    let px = pos.x as f32;
    let py = pos.y as f32;

    let a = dx * dx + dy * dy - dr * dr;
    let b = -2.0 * (px * dx + py * dy + r0 * dr);
    let c = px * px + py * py - r0 * r0;

    let discriminant = b * b - 4.0 * a * c;

    // No solution available.
    if discriminant < 0.0 {
        return None;
    }

    if a.abs() < 1e-6 {
        if b.abs() < 1e-6 {
            return None;
        }

        let t = -c / b;

        if (!min_extend && t < 0.0) || (!max_extend && t > 1.0) {
            return None;
        }

        let r_t = r0 + dr * t;
        if r_t < 0.0 {
            return None;
        }

        return Some(t);
    }

    let sqrt_d = discriminant.sqrt();
    let t1 = (-b - sqrt_d) / (2.0 * a);
    let t2 = (-b + sqrt_d) / (2.0 * a);

    let max = t1.max(t2);
    let mut take_max = Some(max);
    let min = t1.min(t2);
    let mut take_min = Some(min);

    if (!min_extend && min < 0.0) || r0 + dr * min < 0.0 {
        take_min = None;
    }

    if (!max_extend && max > 1.0) || r0 + dr * max < 0.0 {
        take_max = None;
    }

    match (take_min, take_max) {
        (Some(_), Some(max)) => Some(max),
        (Some(min), None) => Some(min),
        (None, Some(max)) => Some(max),
        (None, None) => None,
    }
}
