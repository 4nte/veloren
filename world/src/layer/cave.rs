use super::scatter::close;

use crate::{
    util::{sampler::Sampler, FastNoise, RandomField, RandomPerm, StructureGen2d, LOCALITY},
    Canvas, CanvasInfo, ColumnSample, Land,
};
use common::{
    generation::EntityInfo,
    terrain::{
        quadratic_nearest_point, river_spline_coeffs, Block, BlockKind, CoordinateConversions,
        SpriteKind,
    },
};
use hashbrown::HashMap;
use inline_tweak::tweak_fn;
use noise::NoiseFn;
use rand::prelude::*;
use std::{
    cmp::Ordering,
    f64::consts::PI,
    ops::{Add, Mul, Range, Sub},
};
use vek::*;

const CELL_SIZE: i32 = 1536;

#[derive(Copy, Clone)]
pub struct Node {
    pub wpos: Vec2<i32>,
    pub depth: i32,
}

fn to_cell(wpos: Vec2<i32>, level: u32) -> Vec2<i32> {
    (wpos + (level & 1) as i32 * CELL_SIZE / 4).map(|e| e.div_euclid(CELL_SIZE))
}
fn to_wpos(cell: Vec2<i32>, level: u32) -> Vec2<i32> {
    (cell * CELL_SIZE) - (level & 1) as i32 * CELL_SIZE / 4
}

const AVG_LEVEL_DEPTH: i32 = 120;
const LAYERS: u32 = 4;

fn node_at(cell: Vec2<i32>, level: u32, land: &Land) -> Option<Node> {
    let rand = RandomField::new(37 + level);

    if rand.chance(cell.with_z(0), 0.75) || level == 0 {
        let dx = RandomField::new(38 + level);
        let dy = RandomField::new(39 + level);
        let wpos = to_wpos(cell, level)
            + CELL_SIZE / 4
            + (Vec2::new(dx.get(cell.with_z(0)), dy.get(cell.with_z(0))) % CELL_SIZE as u32 / 2)
                .map(|e| e as i32);
        land.get_chunk_wpos(wpos).and_then(|chunk| {
            let depth = AVG_LEVEL_DEPTH * level as i32 - 6;

            if level > 0
                || (!chunk.near_cliffs()
                    && !chunk.river.near_water()
                    && chunk.sites.is_empty()
                    && land.get_gradient_approx(wpos) < 0.75)
            {
                Some(Node { wpos, depth })
            } else {
                None
            }
        })
    } else {
        None
    }
}

pub fn surface_entrances<'a>(land: &'a Land) -> impl Iterator<Item = Vec2<i32>> + 'a {
    let sz_cells = to_cell(land.size().as_::<i32>().cpos_to_wpos(), 0);
    (0..sz_cells.x + 1)
        .flat_map(move |x| (0..sz_cells.y + 1).map(move |y| Vec2::new(x, y)))
        .filter_map(|cell| Some(tunnel_below_from_cell(cell, 0, land)?.a.wpos))
}

pub struct Tunnel {
    a: Node,
    b: Node,
    curve: f32,
}

impl Tunnel {
    fn ctrl_offset(&self) -> Vec2<f32> {
        let start = self.a.wpos.map(|e| e as f64 + 0.5);
        let end = self.b.wpos.map(|e| e as f64 + 0.5);

        ((end - start) * 0.5 + ((end - start) * 0.5).rotated_z(PI / 2.0) * 6.0 * self.curve as f64)
            .map(|e| e as f32)
    }

    #[tweak_fn]
    fn z_range_at(&self, wposf: Vec2<f64>, info: CanvasInfo) -> Option<(Range<i32>, f64, f64)> {
        let start = self.a.wpos.map(|e| e as f64 + 0.5);
        let end = self.b.wpos.map(|e| e as f64 + 0.5);

        if let Some((t, closest, _)) = quadratic_nearest_point(
            &river_spline_coeffs(start, self.ctrl_offset(), end),
            wposf,
            Vec2::new(start, end),
        ) {
            let dist = closest.distance(wposf);
            let radius = 8.0..64.0;
            if dist < radius.end + 1.0 {
                let horizontal = Lerp::lerp(
                    radius.start,
                    radius.end,
                    (info.index().noise.cave_fbm_nz.get(
                        (wposf.with_z(info.land().get_alt_approx(self.a.wpos) as f64) / 256.0)
                            .into_array(),
                    ) + 0.5)
                        .clamped(0.0, 1.0)
                        .powf(3.0),
                );
                let vertical = Lerp::lerp(
                    radius.start,
                    radius.end,
                    (info.index().noise.cave_fbm_nz.get(
                        (wposf.with_z(info.land().get_alt_approx(self.b.wpos) as f64) / 256.0)
                            .into_array(),
                    ) + 0.5)
                        .clamped(0.0, 1.0)
                        .powf(3.0),
                )
                .clamped(radius.start + 0.0, radius.end);
                let height_here = (1.0 - dist / horizontal).max(0.0).powf(0.3) * vertical;

                if height_here > 0.0 {
                    let z_offs = info
                        .index()
                        .noise
                        .cave_fbm_nz
                        .get((wposf / 512.0).into_array())
                        * 96.0
                        * ((1.0 - (t - 0.5).abs() * 2.0) * 8.0).min(1.0);
                    let alt_here = info.land().get_alt_approx(closest.map(|e| e as i32));
                    let base = (Lerp::lerp(
                        alt_here as f64 - self.a.depth as f64,
                        alt_here as f64 - self.b.depth as f64,
                        t,
                    ) + z_offs)
                        .min(alt_here as f64);
                    Some((
                        (base - height_here * 0.3) as i32..(base + height_here * 1.35) as i32,
                        vertical,
                        horizontal,
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline_tweak::tweak_fn]
    fn biome_at(&self, wpos: Vec3<i32>, info: &CanvasInfo) -> Biome {
        let Some(col) = info.col_or_gen(wpos.xy()) else {
            return Biome::default();
        };

        // Below the ground
        let below = ((col.alt - wpos.z as f32) / AVG_LEVEL_DEPTH as f32).clamped(0.0, 1.0);
        let depth = (col.alt - wpos.z as f32) / (AVG_LEVEL_DEPTH as f32 * LAYERS as f32);

        let humidity = Lerp::lerp(
            col.humidity,
            info.index()
                .noise
                .cave_nz
                .get(wpos.xy().map(|e| e as f64 / 1024.0).into_array()) as f32,
            below,
        );
        let temp = Lerp::lerp(
            col.temp,
            info.index()
                .noise
                .cave_nz
                .get(wpos.xy().map(|e| e as f64 / 2048.0).into_array())
                .mul(2.0)
                .sub(1.0)
                .add(
                    ((col.alt as f64 - wpos.z as f64)
                        / (AVG_LEVEL_DEPTH as f64 * LAYERS as f64 * 0.5))
                        .clamped(0.0, 2.5),
                ) as f32,
            below,
        );
        let mineral = info
            .index()
            .noise
            .cave_nz
            .get(wpos.xy().map(|e| e as f64 / 512.0).into_array())
            .mul(0.5)
            .add(
                ((col.alt as f64 - wpos.z as f64) / (AVG_LEVEL_DEPTH as f64 * LAYERS as f64 * 0.5))
                    .clamped(0.0, 2.0),
            ) as f32;

        let underground = ((col.alt - wpos.z as f32) / 80.0 - 1.0).clamped(0.0, 1.0);

        let [barren, mushroom, fire, leafy, dusty, icy, crystal, sandy] = {
            let barren = 0.01;
            let mushroom = underground * close(humidity, 1.0, 0.75) * close(temp, 0.0, 0.9);
            let fire = underground
                * close(humidity, 0.0, 0.9)
                * close(temp, 2.0, 1.0)
                * close(depth, 1.0, 0.65);
            let leafy = underground * close(humidity, 1.0, 0.85) * close(temp, 0.45, 0.8);
            let dusty =
                close(humidity, 0.0, 0.2) * close(temp, -0.4, 0.5) * close(mineral, 0.0, 0.6);
            let icy = close(temp, -1.0, 0.3);
            let crystal = underground
                * (close(humidity, 0.0, 0.5).max(close(humidity, 1.0, 0.5)))
                * (close(temp, 1.6, 0.9).max(close(temp, -1.0, 0.3)))
                * close(depth, 1.0, 0.6)
                * close(mineral, 2.5, 2.0);
            let sandy = close(humidity, 0.1, 0.2) * close(temp, 0.0, 0.4) * close(depth, 0.4, 0.4);

            let biomes = [barren, mushroom, fire, leafy, dusty, icy, crystal, sandy];
            let max = biomes
                .into_iter()
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                .unwrap();
            biomes.map(|e| (e / max).powf(3.0))
        };

        Biome {
            humidity,
            mineral,
            barren,
            mushroom,
            fire,
            leafy,
            dusty,
            icy,
            crystal,
            sandy,
            depth,
        }
    }

    pub fn nodes(&self) -> (&Node, &Node) { (&self.a, &self.b) }
}

pub(crate) fn tunnels_at<'a>(
    wpos: Vec2<i32>,
    level: u32,
    land: &'a Land,
) -> impl Iterator<Item = Tunnel> + 'a {
    let rand = RandomField::new(37 + level);
    let col_cell = to_cell(wpos - CELL_SIZE / 4, level);
    LOCALITY
        .into_iter()
        .filter_map(move |rpos| {
            let current_cell_pos = col_cell + rpos;
            Some(current_cell_pos).zip(node_at(current_cell_pos, level, land))
        })
        .flat_map(move |(current_cell_pos, current_cell)| {
            [Vec2::new(1, 1), Vec2::new(1, -1)]
                .into_iter()
                .filter(move |rpos| {
                    let mid = (current_cell_pos * 2 + rpos) / 2;
                    rand.chance(mid.with_z(0), 0.5) ^ (rpos.y == -1)
                })
                .chain([Vec2::new(1, 0), Vec2::new(0, 1)])
                .filter_map(move |rpos| {
                    let other_cell_pos = current_cell_pos + rpos;
                    Some(other_cell_pos).zip(node_at(other_cell_pos, level, land))
                })
                .filter(move |(other_cell_pos, _)| {
                    rand.chance((current_cell_pos + other_cell_pos).with_z(7), 0.3)
                })
                .map(move |(_other_cell_pos, other_cell)| Tunnel {
                    a: current_cell,
                    b: other_cell,
                    curve: RandomField::new(13)
                        .get_f32(current_cell.wpos.with_z(0))
                        .powf(0.25)
                        .mul(
                            if RandomField::new(14).chance(current_cell.wpos.with_z(0), 0.5) {
                                1.0
                            } else {
                                -1.0
                            },
                        ),
                })
        })
}

fn tunnel_below_from_cell(cell: Vec2<i32>, level: u32, land: &Land) -> Option<Tunnel> {
    let wpos = to_wpos(cell, level);
    Some(Tunnel {
        a: node_at(to_cell(wpos, level), level, land)?,
        b: node_at(to_cell(wpos + CELL_SIZE / 2, level + 1), level + 1, land)?,
        curve: 0.0,
    })
}

fn tunnels_down_from<'a>(
    wpos: Vec2<i32>,
    level: u32,
    land: &'a Land,
) -> impl Iterator<Item = Tunnel> + 'a {
    let col_cell = to_cell(wpos, level);
    LOCALITY
        .into_iter()
        .filter_map(move |rpos| tunnel_below_from_cell(col_cell + rpos, level, land))
}

pub fn tunnel_bounds_at<'a>(
    wpos2d: Vec2<i32>,
    info: &'a CanvasInfo,
    land: &'a Land,
) -> impl Iterator<Item = (u32, Range<i32>, f64, Tunnel)> + 'a {
    let wposf = wpos2d.map(|e| e as f64 + 0.5);
    info.col_or_gen(wpos2d).into_iter().flat_map(move |col| {
        let col_alt = col.alt;
        let col_water_dist = col.water_dist;
        (1..LAYERS + 1).flat_map(move |level| {
            tunnels_at(wpos2d, level, land)
                .chain(tunnels_down_from(wpos2d, level - 1, land))
                .filter_map(move |tunnel| {
                    let (z_range, vertical, horizontal) = tunnel.z_range_at(wposf, *info)?;
                    let radius = vertical.min(horizontal);
                    // Avoid cave entrances intersecting water
                    let z_range = Lerp::lerp(
                        z_range.end,
                        z_range.start,
                        1.0 - (1.0
                            - ((col_water_dist.unwrap_or(1000.0) - 4.0).max(0.0) / 32.0)
                                .clamped(0.0, 1.0))
                            * (1.0
                                - ((col_alt - z_range.end as f32 - 4.0) / 8.0).clamped(0.0, 1.0)),
                    )..z_range.end;
                    if z_range.end - z_range.start > 0 {
                        Some((level, z_range, radius, tunnel))
                    } else {
                        None
                    }
                })
        })
    })
}

pub fn apply_caves_to(canvas: &mut Canvas, rng: &mut impl Rng) {
    let info = canvas.info();
    let mut structure_cache = HashMap::new();
    // let mut structure_cache =
    // StructureGenCache::<CaveStructure>::new(StructureGen2d::new(34537, 24, 8));
    canvas.foreach_col(|canvas, wpos2d, col| {
        let land = info.land();

        let tunnel_bounds = tunnel_bounds_at(wpos2d, &info, &land).collect::<Vec<_>>();

        // First, clear out tunnels
        for (_, z_range, _, _) in &tunnel_bounds {
            for z in z_range.start..z_range.end.min(col.alt as i32 + 1) {
                canvas.set(wpos2d.with_z(z), Block::air(SpriteKind::Empty));
            }
        }

        for (level, z_range, _radius, tunnel) in tunnel_bounds {
            write_column(
                canvas,
                col,
                level,
                wpos2d,
                z_range.clone(),
                tunnel,
                &mut structure_cache,
                rng,
            );
        }
    });
}

#[derive(Default)]
struct Biome {
    humidity: f32,
    barren: f32,
    mineral: f32,
    mushroom: f32,
    fire: f32,
    leafy: f32,
    dusty: f32,
    icy: f32,
    crystal: f32,
    sandy: f32,
    depth: f32,
}

enum CaveStructure {
    Mushroom(Mushroom),
    Crystal(CrystalCluster),
    Flower(Flower),
}

struct Mushroom {
    pos: Vec3<i32>,
    stalk: f32,
    head_color: Rgb<u8>,
}

struct Crystal {
    dir: Vec3<f32>,
    length: f32,
    radius: f32,
}

struct CrystalCluster {
    pos: Vec3<i32>,
    crystals: Vec<Crystal>,
    color: Rgb<u8>,
}

struct Flower {
    pos: Vec3<i32>,
    stalk: f32,
    petals: usize,
}

#[inline_tweak::tweak_fn]
fn write_column<R: Rng>(
    canvas: &mut Canvas,
    col: &ColumnSample,
    level: u32,
    wpos2d: Vec2<i32>,
    z_range: Range<i32>,
    tunnel: Tunnel,
    structure_cache: &mut HashMap<(Vec3<i32>, Vec2<i32>), Option<CaveStructure>>,
    rng: &mut R,
) {
    let info = canvas.info();

    // Exposed to the sky, or some other void above
    let void_above = !canvas.get(wpos2d.with_z(z_range.end)).is_filled();
    let void_below = !canvas.get(wpos2d.with_z(z_range.start - 1)).is_filled();
    // Exposed to the sky
    let sky_above = z_range.end as f32 > col.alt;

    let biome = tunnel.biome_at(wpos2d.with_z(z_range.start), &info);

    let stalactite = {
        let cavern_height = (z_range.end - z_range.start) as f64;
        info
            .index()
            .noise
            .cave_nz
            .get(wpos2d.map(|e| e as f64 / 16.0).into_array())
            .sub(0.5)
            .max(0.0)
            .mul(2.0)
            // No stalactites near entrances
            .mul(((col.alt as f64 - z_range.end as f64) / 8.0).clamped(0.0, 1.0))
            .mul(8.0 + cavern_height * (0.4 + (biome.sandy as f64 - 0.5).max(0.0)))
    };

    let basalt = if biome.fire > 0.0 {
        let cavern_height = (z_range.end - z_range.start) as f64;
        info.index()
            .noise
            .cave_nz
            .get(wpos2d.map(|e| e as f64 / 48.0).into_array())
            .sub(0.5)
            .max(0.0)
            .mul(6.0 + cavern_height * 0.5)
            .mul(biome.fire as f64)
    } else {
        0.0
    };

    let lava = if biome.fire > 0.0 {
        info.index()
            .noise
            .cave_nz
            .get(wpos2d.map(|e| e as f64 / 64.0).into_array())
            .sub(0.5)
            .abs()
            .sub(0.2)
            .min(0.0)
            // .mul((biome.temp as f64 - 1.5).mul(30.0).clamped(0.0, 1.0))
            .mul((biome.fire as f64 - 0.5).mul(30.0).clamped(0.0, 1.0))
            .mul(64.0)
            .max(-32.0)
    } else {
        0.0
    };

    let bump = {
        let cavern_height = (z_range.end - z_range.start) as f64;
        info.index()
            .noise
            .cave_nz
            .get(wpos2d.map(|e| e as f64 / 8.0).into_array())
            .add(1.0)
            .mul(0.5)
            .mul(((col.alt as f64 - z_range.end as f64) / 16.0).clamped(0.0, 1.0))
            .mul({
                let (val, total) = [
                    (biome.sandy as f64 - 0.2, 0.9),
                    (biome.dusty as f64 - 0.1, 0.5),
                    (biome.icy as f64 - 0.1, 0.1),
                    (biome.leafy as f64 - 0.2, 0.8),
                    (biome.barren as f64 - 0.15, 0.6),
                    (biome.mushroom as f64 - 0.3, 0.2),
                    (biome.fire as f64 - 0.5, 0.2),
                ]
                .into_iter()
                .fold((0.0, 0.0), |a, x| (a.0 + x.0.max(0.0) * x.1, a.1 + x.1));
                val / total
            })
            .mul(0.0 + cavern_height * 0.4)
    };

    let rand = RandomField::new(37 + level);

    let is_ice = biome.icy + col.marble * 0.2 > 0.5 && col.marble > 0.6;

    let dirt = 1 + (!is_ice) as i32;
    let bedrock = z_range.start + lava as i32;
    let base = bedrock + (stalactite * 0.4) as i32 + bump as i32;
    let floor = base + dirt;
    let ceiling = z_range.end - stalactite as i32;

    let mut get_structure = |wpos: Vec3<i32>, dynamic_rng: &mut R| {
        for (wpos2d, seed) in StructureGen2d::new(34537, 24, 8).get(wpos.xy()) {
            let structure = if let Some(structure) = structure_cache
                .entry((tunnel.a.wpos.with_z(tunnel.a.depth), wpos2d))
                .or_insert_with(|| {
                    let mut rng = RandomPerm::new(seed);
                    let (z_range, vertical, horizontal) =
                        tunnel.z_range_at(wpos2d.map(|e| e as f64 + 0.5), info)?;
                    let radius = vertical.min(horizontal);
                    let pos = wpos2d.with_z(z_range.start);

                    if rng.gen_bool(0.5 * close(radius as f32, 64.0, 48.0) as f64)
                        && tunnel.biome_at(pos, &info).mushroom > 0.5
                        && !tunnel_bounds_at(pos.xy(), &info, &info.land())
                            .any(|(_, z_range, _, _)| z_range.contains(&(z_range.start - 1)))
                    {
                        let purp = rng.gen_range(0..50);
                        Some(CaveStructure::Mushroom(Mushroom {
                            pos,
                            stalk: 8.0
                                + rng.gen::<f32>().powf(2.0)
                                    * (z_range.end - z_range.start - 8) as f32
                                    * 0.75,
                            head_color: Rgb::new(
                                40 + purp,
                                rng.gen_range(60..120),
                                rng.gen_range(80..200) + purp,
                            ),
                        }))
                    } else if rng
                        .gen_bool(0.8 * close(tunnel.biome_at(pos, &info).crystal, 1.0, 0.7) as f64)
                        && !tunnel_bounds_at(pos.xy(), &info, &info.land())
                            .any(|(_, z_range, _, _)| z_range.contains(&(z_range.start - 1)))
                    {
                        let colors = [
                            Rgb::new(209, 106, 255),
                            Rgb::new(187, 86, 240),
                            Rgb::new(251, 238, 255),
                            Rgb::new(243, 204, 255),
                        ];
                        let on_ground = rng.gen_bool(0.6);
                        let pos = wpos2d.with_z(if on_ground {
                            z_range.start
                        } else {
                            z_range.end
                        });

                        let mut crystals: Vec<Crystal> = Vec::new();

                        let max_length = (48.0 * close(radius as f32, 64.0, 52.0)).max(12.0);
                        let main_length = rng.gen_range(8.0..max_length);
                        let main_radius = Lerp::lerp_unclamped(
                            2.0,
                            4.5,
                            main_length / max_length + rng.gen_range(-0.1..0.1),
                        );

                        let main_dir = Vec3::new(
                            rng.gen_range(-3.0..3.0),
                            rng.gen_range(-3.0..3.0),
                            rng.gen_range(0.0..10.0) * if on_ground { 1.0 } else { -1.0 },
                        )
                        .normalized();

                        let main_crystal = Crystal {
                            dir: main_dir,
                            length: main_length,
                            radius: main_radius,
                        };
                        crystals.push(main_crystal);

                        (0..4).for_each(|_| {
                            let side_radius = (main_radius * rng.gen_range(0.5..0.8)).max(1.0);
                            let side_length = main_length * rng.gen_range(0.3..0.8);
                            let side_crystal = Crystal {
                                dir: Vec3::new(
                                    rng.gen_range(-1.0..1.0),
                                    rng.gen_range(-1.0..1.0),
                                    (main_dir.z + rng.gen_range(-0.2..0.2)).clamped(0.0, 1.0),
                                ),
                                length: side_length,
                                radius: side_radius,
                            };
                            crystals.push(side_crystal);
                        });

                        let mut color: Rgb<u8> = *(colors.choose(&mut rng).unwrap());
                        if tunnel.biome_at(pos, &info).icy > 0.125 {
                            color.r = color.r.saturating_sub(150u8);
                            color.g = color.g.saturating_sub(40u8);
                            color.b = color.b.saturating_add(0u8);
                        }
                        Some(CaveStructure::Crystal(CrystalCluster {
                            pos,
                            crystals,
                            color,
                        }))
                    } else if rng.gen_bool(0.5 * close(radius as f32, 64.0, 48.0) as f64)
                        && tunnel.biome_at(pos, &info).leafy > 0.9
                        && !tunnel_bounds_at(pos.xy(), &info, &info.land())
                            .any(|(_, z_range, _, _)| z_range.contains(&(z_range.start - 1)))
                    {
                        Some(CaveStructure::Flower(Flower {
                            pos,
                            stalk: 8.0
                                + rng.gen::<f32>().powf(2.0)
                                    * (z_range.end - z_range.start - 8) as f32
                                    * 0.75,
                            petals: rng.gen_range(1..5) * 2 + 1,
                        }))
                    } else {
                        None
                    }
                }) {
                structure
            } else {
                continue;
            };

            match structure {
                CaveStructure::Mushroom(mushroom) => {
                    let wposf = wpos.map(|e| e as f64);
                    let warp_freq = 1.0 / 32.0;
                    let warp_amp = Vec3::new(12.0, 12.0, 12.0);
                    let wposf_warped = wposf.map(|e| e as f32)
                        + Vec3::new(
                            FastNoise::new(seed).get(wposf * warp_freq),
                            FastNoise::new(seed + 1).get(wposf * warp_freq),
                            FastNoise::new(seed + 2).get(wposf * warp_freq),
                        ) * warp_amp
                            * (wposf.z as f32 - mushroom.pos.z as f32)
                                .mul(0.1)
                                .clamped(0.0, 1.0);

                    let rpos = wposf_warped - mushroom.pos.map(|e| e as f32);

                    let stalk_radius = 2.5f32;
                    let head_radius = 12.0f32;
                    let head_height = 14.0;

                    let dist_sq = rpos.xy().magnitude_squared();
                    if dist_sq < head_radius.powi(2) {
                        let dist = dist_sq.sqrt();
                        let head_dist = ((rpos - Vec3::unit_z() * mushroom.stalk)
                            / Vec2::broadcast(head_radius).with_z(head_height))
                        .magnitude();

                        let stalk =
                            mushroom.stalk + Lerp::lerp(head_height * 0.5, 0.0, dist / head_radius);

                        // Head
                        if rpos.z > stalk
                            && rpos.z <= mushroom.stalk + head_height
                            && dist
                                < head_radius
                                    * (1.0 - (rpos.z - mushroom.stalk) / head_height).powf(0.125)
                        {
                            if head_dist < 0.85 {
                                let radial = (rpos.x.atan2(rpos.y) * 10.0).sin() * 0.5 + 0.5;
                                return Some(Block::new(
                                    BlockKind::GlowingMushroom,
                                    Rgb::new(
                                        30,
                                        50 + (radial * 100.0) as u8,
                                        100 - (radial * 50.0) as u8,
                                    ),
                                ));
                            } else if head_dist < 1.0 {
                                return Some(Block::new(BlockKind::Wood, mushroom.head_color));
                            }
                        }

                        if rpos.z <= mushroom.stalk + head_height - 1.0
                            && dist_sq
                                < (stalk_radius * Lerp::lerp(1.5, 0.75, rpos.z / mushroom.stalk))
                                    .powi(2)
                        {
                            // Stalk
                            return Some(Block::new(BlockKind::Wood, Rgb::new(25, 60, 90)));
                        } else if ((mushroom.stalk - 0.1)..(mushroom.stalk + 0.9)).contains(&rpos.z) // Hanging orbs
                    && dist > head_radius * 0.85
                    && dynamic_rng.gen_bool(0.1)
                        {
                            use SpriteKind::*;
                            let sprites = if dynamic_rng.gen_bool(0.1) {
                                &[Beehive, Lantern] as &[_]
                            } else {
                                &[Orb, CavernMycelBlue, CavernMycelBlue] as &[_]
                            };
                            return Some(Block::air(*sprites.choose(dynamic_rng).unwrap()));
                        }
                    }
                },
                CaveStructure::Crystal(cluster) => {
                    let wposf = wpos.map(|e| e as f32);
                    let cluster_pos = cluster.pos.map(|e| e as f32);
                    for crystal in &cluster.crystals {
                        let line = LineSegment3 {
                            start: cluster_pos,
                            end: cluster_pos + crystal.dir * crystal.length,
                        };

                        let projected = line.projected_point(wposf);
                        let dist_sq = projected.distance_squared(wposf);
                        if dist_sq < crystal.radius.powi(2) {
                            let rpos = wposf - cluster_pos;
                            let line_length = line.start.distance_squared(line.end);
                            let taper = if line_length < 0.001 {
                                0.0
                            } else {
                                rpos.dot(line.end - line.start) / line_length
                            };

                            let peak_cutoff = 0.8;
                            let taper_factor = 0.55;
                            let peak_taper = 0.3;

                            let crystal_radius = if taper > peak_cutoff {
                                let taper = (taper - peak_cutoff) * 5.0;
                                Lerp::lerp(
                                    crystal.radius * taper_factor,
                                    crystal.radius * peak_taper,
                                    taper,
                                )
                            } else {
                                let taper = taper * 1.25;
                                Lerp::lerp(crystal.radius, crystal.radius * taper_factor, taper)
                            };

                            if dist_sq < crystal_radius.powi(2) {
                                return Some(Block::new(BlockKind::GlowingRock, cluster.color));
                            }
                        }
                    }
                },
                CaveStructure::Flower(flower) => {
                    let wposf = wpos.map(|e| e as f64);
                    let warp_freq = 1.0 / 32.0;
                    let warp_amp = Vec3::new(3.0, 3.0, 3.0);
                    let wposf_warped = wposf.map(|e| e as f32)
                        + Vec3::new(
                            FastNoise::new(seed).get(wposf * warp_freq),
                            FastNoise::new(seed + 1).get(wposf * warp_freq),
                            FastNoise::new(seed + 2).get(wposf * warp_freq),
                        ) * warp_amp
                            * (wposf.z as f32 - flower.pos.z as f32)
                                .mul(0.1)
                                .clamped(0.0, 1.0);

                    let rpos = wposf_warped - flower.pos.map(|e| e as f32);

                    let stalk_radius = 2.5f32;
                    let petal_radius = 12.0f32;
                    let petal_height = 8.0;
                    let petal_thickness = 3.0;

                    let dist = rpos.xy().magnitude_squared();
                    let petal_radius = petal_radius.powi(2);
                    if dist < petal_radius {
                        let petal_height_at = (dist / petal_radius).powf(0.5) * petal_height;
                        if rpos.z > flower.stalk + petal_height_at
                            && rpos.z <= flower.stalk + petal_height_at + petal_thickness
                        {
                            let away = dist / petal_radius;
                            if away > 0.2 {
                                let near = (rpos.x.atan2(rpos.y))
                                    .rem_euclid(std::f32::consts::TAU / flower.petals as f32);
                                let inset = close(near, -1.0, 0.85).max(close(near, 1.0, 0.85));
                                if dist / petal_radius < inset {
                                    return Some(Block::new(
                                        BlockKind::Wood,
                                        Rgb::new(240, 50, 50),
                                    ));
                                }
                            } else {
                                return Some(Block::new(BlockKind::Wood, Rgb::new(240, 50, 50)));
                            }
                        }

                        if rpos.z <= flower.stalk + 3.0
                            && dist
                                < (stalk_radius * Lerp::lerp(1.5, 0.75, rpos.z / flower.stalk))
                                    .powi(2)
                        {
                            if flower.stalk - rpos.z - 0.0 + 0.0 >= 0.0 {
                                return Some(Block::new(BlockKind::Wood, Rgb::new(40, 244, 150)));
                            }
                            // Stalk
                            return Some(Block::new(BlockKind::Wood, Rgb::new(150, 244, 40)));
                        }
                    }
                },
            }
        }
        None
    };

    for z in bedrock..z_range.end {
        let wpos = wpos2d.with_z(z);
        let mut try_spawn_entity = false;
        canvas.map_resource(wpos, |_block| {
            if z < z_range.start - 4 && !void_below {
                Block::new(BlockKind::Lava, Rgb::new(255, 65, 0))
            } else if basalt > 0.0
                && z < bedrock / 6 * 6
                    + 2
                    + basalt as i32 / 4 * 4
                    + (RandomField::new(77)
                        .get_f32(((wpos2d + Vec2::new(wpos2d.y, -wpos2d.x) / 2) / 4).with_z(0))
                        * 6.0)
                        .floor() as i32
                && !void_below
            {
                Block::new(BlockKind::Rock, Rgb::new(50, 35, 75))
            } else if (z < base && !void_below) || (z >= ceiling && !void_above) {
                let stalactite: Rgb<i16> = Lerp::lerp(
                    Lerp::lerp(
                        Lerp::lerp(
                            Lerp::lerp(
                                Rgb::new(80, 100, 150),
                                Rgb::new(0, 75, 200),
                                biome.mushroom,
                            ),
                            Lerp::lerp(
                                Rgb::new(100, 40, 40),
                                Rgb::new(100, 75, 100),
                                col.marble_small,
                            ),
                            biome.fire,
                        ),
                        Lerp::lerp(
                            Rgb::new(238, 198, 139),
                            Rgb::new(111, 99, 64),
                            col.marble_mid,
                        ),
                        biome.sandy,
                    ),
                    Lerp::lerp(Rgb::new(100, 150, 255), Rgb::new(100, 120, 255), col.marble),
                    biome.icy,
                );
                Block::new(
                    if rand.chance(wpos, (biome.mushroom * biome.mineral).max(biome.icy)) {
                        BlockKind::GlowingWeakRock
                    } else if rand.chance(wpos, biome.sandy) {
                        BlockKind::Sand
                    } else {
                        BlockKind::WeakRock
                    },
                    stalactite.map(|e| e as u8),
                )
            } else if z >= base && z < floor && !void_below && !sky_above {
                let (net_col, total) = [
                    (
                        Lerp::lerp(Rgb::new(40, 20, 0), Rgb::new(80, 80, 30), col.marble_small),
                        0.05,
                    ),
                    (
                        Lerp::lerp(Rgb::new(50, 50, 75), Rgb::new(75, 75, 50), col.marble_mid),
                        biome.dusty,
                    ),
                    (
                        Lerp::lerp(Rgb::new(20, 65, 175), Rgb::new(20, 100, 80), col.marble_mid),
                        biome.mushroom,
                    ),
                    (
                        Lerp::lerp(Rgb::new(120, 50, 20), Rgb::new(50, 5, 40), col.marble_small),
                        biome.fire,
                    ),
                    (
                        Lerp::lerp(
                            Rgb::new(0, 100, 50),
                            Rgb::new(80, 100, 20),
                            col.marble_small,
                        ),
                        biome.leafy,
                    ),
                    (Rgb::new(170, 195, 255), biome.icy),
                    (
                        Lerp::lerp(
                            Rgb::new(105, 25, 131),
                            Rgb::new(251, 238, 255),
                            col.marble_mid,
                        ),
                        biome.crystal,
                    ),
                    (
                        Lerp::lerp(
                            Rgb::new(201, 174, 116),
                            Rgb::new(244, 239, 227),
                            col.marble_small,
                        ),
                        biome.sandy,
                    ),
                ]
                .into_iter()
                .fold((Rgb::<f32>::zero(), 0.0), |a, x| {
                    (a.0 + x.0.map(|e| e as f32) * x.1, a.1 + x.1)
                });
                let surf_color = net_col.map(|e| (e / total) as u8);

                if is_ice {
                    Block::new(BlockKind::Ice, Rgb::new(120, 160, 255))
                } else {
                    Block::new(
                        if biome.mushroom.max(biome.leafy) > 0.5 {
                            BlockKind::Grass
                        } else if biome.icy > 0.5 {
                            BlockKind::Snow
                        } else if biome.fire > 0.5 {
                            BlockKind::Rock
                        } else if biome.crystal > 0.5 {
                            BlockKind::GlowingRock
                        } else {
                            BlockKind::Sand
                        },
                        surf_color,
                    )
                }
            } else if let Some(sprite) = (z == floor && !void_below && !sky_above)
                .then(|| {
                    if rand.chance(wpos2d.with_z(1), biome.mushroom * 0.05) {
                        [
                            (SpriteKind::CaveMushroom, 0.15),
                            (SpriteKind::Mushroom, 0.25),
                            (SpriteKind::GrassBlue, 1.0),
                            (SpriteKind::CavernGrassBlueShort, 1.0),
                            (SpriteKind::CavernGrassBlueMedium, 1.0),
                            (SpriteKind::CavernGrassBlueLong, 1.0),
                            (SpriteKind::Moonbell, 0.01),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .map(|s| s.0)
                    } else if rand.chance(wpos2d.with_z(15), biome.leafy * 0.05) {
                        [
                            (SpriteKind::LongGrass, 1.0),
                            (SpriteKind::MediumGrass, 2.0),
                            (SpriteKind::ShortGrass, 2.0),
                            (SpriteKind::JungleFern, 0.5),
                            (SpriteKind::JungleLeafyPlant, 0.5),
                            (SpriteKind::JungleRedGrass, 0.35),
                            (SpriteKind::Mushroom, 0.15),
                            (SpriteKind::EnsnaringVines, 0.2),
                            (SpriteKind::Fern, 0.75),
                            (SpriteKind::LeafyPlant, 0.8),
                            (SpriteKind::Twigs, 0.07),
                            (SpriteKind::Wood, 0.03),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .map(|s| s.0)
                    } else if rand.chance(wpos2d.with_z(2), biome.dusty.max(biome.sandy) * 0.01) {
                        [
                            (SpriteKind::Bones, 0.5),
                            (SpriteKind::Stones, 1.5),
                            (SpriteKind::DeadBush, 1.0),
                            (SpriteKind::EnsnaringWeb, 0.5),
                            (SpriteKind::Mud, 0.025),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .map(|s| s.0)
                    } else if rand.chance(wpos2d.with_z(14), biome.barren * 0.003) {
                        [
                            (SpriteKind::Welwitch, 0.5),
                            (SpriteKind::DeadBush, 1.5),
                            (SpriteKind::Crate, 0.005),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .map(|s| s.0)
                    } else if rand.chance(
                        wpos2d.with_z(3),
                        close(biome.humidity, 0.0, 0.5) * biome.mineral.max(biome.crystal) * 0.005,
                    ) {
                        Some(SpriteKind::CrystalLow)
                    } else if rand.chance(wpos2d.with_z(13), biome.fire * 0.001) {
                        [
                            (SpriteKind::Pyrebloom, 0.3),
                            (SpriteKind::Bloodstone, 0.3),
                            (SpriteKind::Gold, 0.15),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .map(|s| s.0)
                    } else if rand.chance(wpos2d.with_z(5), 0.0025) {
                        [
                            (Some(SpriteKind::VeloriteFrag), 0.3),
                            (Some(SpriteKind::AmethystSmall), 0.3),
                            (Some(SpriteKind::TopazSmall), 0.3),
                            (Some(SpriteKind::DiamondSmall), 0.04),
                            (Some(SpriteKind::RubySmall), 0.1),
                            (Some(SpriteKind::EmeraldSmall), 0.08),
                            (Some(SpriteKind::SapphireSmall), 0.08),
                            (Some(SpriteKind::Velorite), 0.15),
                            (Some(SpriteKind::Amethyst), 0.15),
                            (Some(SpriteKind::Topaz), 0.15),
                            (Some(SpriteKind::Diamond), 0.02),
                            (Some(SpriteKind::Ruby), 0.05),
                            (Some(SpriteKind::Emerald), 0.04),
                            (Some(SpriteKind::Sapphire), 0.04),
                            (None, 10.0),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .and_then(|s| s.0)
                    } else if rand.chance(wpos2d.with_z(6), 0.0002) {
                        [
                            (Some(SpriteKind::DungeonChest0), 1.0),
                            (Some(SpriteKind::DungeonChest1), 0.3),
                            (Some(SpriteKind::DungeonChest2), 0.1),
                            (Some(SpriteKind::DungeonChest3), 0.03),
                            (Some(SpriteKind::DungeonChest4), 0.01),
                            (Some(SpriteKind::DungeonChest5), 0.003),
                            (None, 1.0),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .and_then(|s| s.0)
                    } else if rand.chance(wpos2d.with_z(7), 0.01) {
                        let shallow = close(biome.depth, 0.0, 0.4);
                        let middle = close(biome.depth, 0.5, 0.4);
                        //let deep = close(biome.depth, 1.0, 0.4); // TODO: Use this for deep only
                        // things
                        [
                            (Some(SpriteKind::Stones), 1.5),
                            (Some(SpriteKind::Copper), shallow),
                            (Some(SpriteKind::Tin), shallow),
                            (Some(SpriteKind::Iron), shallow * 0.5),
                            (Some(SpriteKind::Coal), middle * 0.25),
                            (Some(SpriteKind::Cobalt), middle * 0.1),
                            (Some(SpriteKind::Silver), middle * 0.05),
                            (None, 10.0),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .and_then(|s| s.0)
                    } else {
                        try_spawn_entity = true;
                        None
                    }
                })
                .flatten()
            {
                Block::air(sprite)
            } else if let Some(sprite) = (z == ceiling - 1 && !void_above)
                .then(|| {
                    if rand.chance(wpos2d.with_z(3), biome.mushroom * 0.01) {
                        Some(
                            *[
                                SpriteKind::CavernMycelBlue,
                                SpriteKind::CeilingMushroom,
                                SpriteKind::Orb,
                            ]
                            .choose(rng)
                            .unwrap(),
                        )
                    } else if rand.chance(wpos2d.with_z(4), biome.leafy * 0.015) {
                        [
                            (SpriteKind::Liana, 1.0),
                            (SpriteKind::Orb, 0.35),
                            (SpriteKind::CrystalHigh, 0.1),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .ok()
                        .map(|s| s.0)
                    } else if rand
                        .chance(wpos2d.with_z(5), biome.mineral.max(biome.crystal) * 0.0075)
                    {
                        Some(*[SpriteKind::CrystalHigh].choose(rng).unwrap())
                    } else {
                        None
                    }
                })
                .flatten()
            {
                Block::air(sprite)
            } else {
                get_structure(wpos, rng).unwrap_or(Block::air(SpriteKind::Empty))
            }
        });

        if try_spawn_entity {
            apply_entity_spawns(canvas, wpos, &biome, rng);
        }
    }
}

fn apply_entity_spawns<R: Rng>(canvas: &mut Canvas, wpos: Vec3<i32>, biome: &Biome, rng: &mut R) {
    if RandomField::new(canvas.info().index().seed).chance(wpos, 0.05) {
        if let Some(entity_asset) = [
            // Mushroom biome
            (
                Some("common.entity.wild.peaceful.truffler"),
                (biome.mushroom + 0.02) * 0.35,
            ),
            (
                Some("common.entity.wild.peaceful.fungome"),
                (biome.mushroom + 0.02) * 0.5,
            ),
            (
                Some("common.entity.wild.peaceful.bat"),
                (biome.mushroom + 0.1) * 0.25,
            ),
            // Leafy biome
            (
                Some("common.entity.wild.peaceful.holladon"),
                (biome.leafy.max(biome.barren) + 0.1) * 0.5,
            ),
            (
                Some("common.entity.wild.peaceful.turtle"),
                (biome.leafy + 0.05) * 0.5,
            ),
            (
                Some("common.entity.wild.aggressive.rootsnapper"),
                (biome.leafy + 0.05) * 0.02,
            ),
            (
                Some("common.entity.wild.peaceful.axolotl"),
                (biome.leafy + 0.05) * 0.5,
            ),
            (
                Some("common.entity.wild.aggressive.maneater"),
                (biome.leafy + 0.0) * 0.05,
            ),
            (
                Some("common.entity.wild.aggressive.batfox"),
                (biome.leafy.max(biome.barren) + 0.15) * 0.35,
            ),
            (
                Some("common.entity.wild.aggressive.rocksnapper"),
                (biome.leafy.max(biome.barren) + 0.1) * 0.08,
            ),
            (
                Some("common.entity.wild.aggressive.cave_salamander"),
                (biome.leafy + 0.1) * 0.2,
            ),
            (
                Some("common.entity.wild.aggressive.asp"),
                (biome.leafy + 0.1) * 0.1,
            ),
            (
                Some("common.entity.wild.aggressive.swamp_troll"),
                (biome.leafy + 0.0) * 0.1,
            ),
            (
                Some("common.entity.wild.peaceful.bat"),
                (biome.leafy + 0.1) * 0.25,
            ),
            // Dusty biome
            (
                Some("common.entity.wild.aggressive.dodarock"),
                (biome.dusty.max(biome.barren) + 0.05) * 0.05,
            ),
            (
                Some("common.entity.wild.aggressive.cave_spider"),
                (biome.dusty + 0.0) * 0.05,
            ),
            (
                Some("common.entity.wild.aggressive.cave_troll"),
                (biome.dusty + 0.1) * 0.05,
            ),
            (
                Some("common.entity.wild.aggressive.antlion"),
                (biome.dusty.min(biome.depth) + 0.0) * 0.01,
            ),
            (
                Some("common.entity.wild.peaceful.rat"),
                (biome.dusty.max(biome.barren) + 0.15) * 0.3,
            ),
            (
                Some("common.entity.wild.peaceful.bat"),
                (biome.dusty.max(biome.barren) + 0.1) * 0.25,
            ),
            // Icy biome
            (
                Some("common.entity.wild.aggressive.blue_oni"),
                (biome.icy + 0.0) * 0.03,
            ),
            (
                Some("common.entity.wild.aggressive.icedrake"),
                (biome.icy + 0.0) * 0.04,
            ),
            (
                Some("common.entity.wild.aggressive.wendigo"),
                (biome.icy.min(biome.depth) + 0.0) * 0.02,
            ),
            // Lava biome
            (
                Some("common.entity.wild.aggressive.lavadrake"),
                (biome.fire + 0.0) * 0.15,
            ),
            (
                Some("common.entity.wild.peaceful.crawler_molten"),
                (biome.fire + 0.0) * 0.35,
            ),
            (
                Some("common.entity.wild.aggressive.red_oni"),
                (biome.fire + 0.0) * 0.05,
            ),
            // With depth
            (
                Some("common.entity.wild.aggressive.black_widow"),
                (biome.depth + 0.0) * 0.01,
            ),
            (
                Some("common.entity.wild.aggressive.ogre"),
                (biome.depth + 0.0) * 0.02,
            ),
            (
                Some("common.entity.wild.aggressive.basilisk"),
                (biome.depth + 0.1) * 0.005,
            ),
            (None, 100.0),
        ]
        .choose_weighted(rng, |(_, w)| *w)
        .ok()
        .and_then(|s| s.0)
        {
            canvas.spawn(EntityInfo::at(wpos.map(|e| e as f32)).with_asset_expect(
                entity_asset,
                rng,
                None,
            ));
        }
    }

    // FIXME: Add back waypoints once caves are not impossible to escape.
    /* // Occasionally place down a waypoint
    if RandomField::new(canvas.info().index().seed).chance(wpos, 0.000005) {
        canvas.spawn(EntityInfo::at(wpos.map(|e| e as f32)).into_waypoint());
    } */
}
