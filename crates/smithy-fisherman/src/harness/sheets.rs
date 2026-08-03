//! Contact sheets with labels *in* the image.
//!
//! Labels go in the PNG, not stdout: a vision model that has to count tiles
//! will miscount, and that is budget spent on a wrong conclusion.

use kurbo::{BezPath, Rect, Shape};

use crate::fisherman::{self as f, scene_at, Scene, BUILD_SECONDS};
use crate::routine::{Doing, Place};
use crate::Ink;

use super::font::draw_label;
use super::raster::{OffsetInk, PixmapInk, STEEL_BODY, STEEL_DEEP};
use super::{height, launched_built, BAND, DAY, SUNRISE, SUNSET, WIDTH};

fn doing_label(d: Doing) -> &'static str {
    match d {
        Doing::Sleeping => "SLEEPING",
        Doing::Waking => "WAKING",
        Doing::Exercising => "EXERCISING",
        Doing::Coffee => "COFFEE",
        Doing::Gardening => "GARDENING",
        Doing::Fishing => "FISHING",
        Doing::Cooking => "COOKING",
        Doing::Eating => "EATING",
        Doing::Siesta => "SIESTA",
        Doing::Reading => "READING",
        Doing::Walking => "WALKING",
        Doing::Smoking => "SMOKING",
    }
}

/// 96 tiles — every 15 minutes of a simulated day.
pub fn day_sheet(out_dir: &std::path::Path) {
    let cols = 12u32;
    let rows = 8u32; // 96 / 12
    let tile_w = WIDTH as u32;
    let tile_h = height() as u32;
    let gap = 4u32;
    let sheet_w = cols * tile_w + (cols - 1) * gap;
    let sheet_h = rows * tile_h + (rows - 1) * gap;
    let mut sheet = PixmapInk::new(sheet_w, sheet_h, STEEL_DEEP);

    for i in 0..96u32 {
        let minutes = i * 15;
        let hours = minutes as f64 / 60.0;
        let col = i % cols;
        let row = i / cols;
        let ox = (col * (tile_w + gap)) as f64;
        let oy = (row * (tile_h + gap)) as f64;

        let scene = scene_at(
            WIDTH,
            height(),
            BAND,
            hours,
            SUNRISE,
            SUNSET,
            DAY,
            launched_built(),
            (hours * 18000.0) as u64,
        );
        paint_tile(&mut sheet, ox, oy, &scene);

        let h = (minutes / 60) as u32;
        let m = minutes % 60;
        let label = format!(
            "{} {:02}:{:02}",
            doing_label(scene.doing),
            h,
            m
        );
        draw_label(
            &mut sheet,
            ox as i32 + 4,
            oy as i32 + 4,
            &label,
            1,
        );
    }

    sheet.save(&out_dir.join("day.png"));
}

/// All 12 Doing states in place with props and lighting.
pub fn scenes_sheet(out_dir: &std::path::Path) {
    // Typical place for each activity (completion=1.0). Includes the five the
    // old sheet missed: Waking, Exercising, Eating, Siesta, Smoking.
    let tiles: &[(&str, Doing, Place, Place, f64)] = &[
        ("WAKING", Doing::Waking, Place::Hut, Place::Hut, 0.45),
        ("EXERCISING", Doing::Exercising, Place::Doorstep, Place::Hut, 0.5),
        ("COFFEE", Doing::Coffee, Place::Doorstep, Place::Hut, 0.5),
        ("GARDENING", Doing::Gardening, Place::Garden, Place::Doorstep, 0.5),
        ("FISHING", Doing::Fishing, Place::Perch, Place::Garden, 0.5),
        ("COOKING", Doing::Cooking, Place::Fire, Place::Perch, 0.5),
        ("EATING", Doing::Eating, Place::Doorstep, Place::Fire, 0.5),
        ("SIESTA", Doing::Siesta, Place::Hut, Place::Doorstep, 0.5),
        ("WALKING", Doing::Walking, Place::Hut, Place::Garden, 0.5),
        ("READING", Doing::Reading, Place::Hut, Place::Doorstep, 0.5),
        ("SMOKING", Doing::Smoking, Place::Garden, Place::Garden, 0.5),
        ("SLEEPING", Doing::Sleeping, Place::Hut, Place::Doorstep, 0.5),
    ];

    let tile_w = WIDTH as u32;
    let tile_h = height() as u32;
    let gap = 6u32;
    let sheet = {
        let h = tiles.len() as u32 * (tile_h + gap) - gap;
        PixmapInk::new(tile_w, h, STEEL_DEEP)
    };
    let mut sheet = sheet;

    for (i, (label, doing, place, previous, progress)) in tiles.iter().enumerate() {
        let oy = (i as u32 * (tile_h + gap)) as f64;
        let scene = Scene {
            width: WIDTH,
            height: height(),
            band: BAND,
            doing: *doing,
            place: *place,
            previous: *previous,
            progress: *progress,
            completion: 1.0,
            frame: 40 + i as u64,
            seconds: 8.0 + i as f64,
        };
        paint_tile(&mut sheet, 0.0, oy, &scene);
        draw_label(&mut sheet, 4, oy as i32 + 4, label, 2);
    }

    sheet.save(&out_dir.join("scenes.png"));
}

/// Hut going up across BUILD_SECONDS.
pub fn build_sheet(out_dir: &std::path::Path) {
    let frames = 10u32;
    let tile_w = WIDTH as u32;
    let tile_h = height() as u32;
    let gap = 4u32;
    let mut sheet = PixmapInk::new(
        tile_w,
        frames * (tile_h + gap) - gap,
        STEEL_DEEP,
    );

    for i in 0..frames {
        let t = i as f64 / (frames - 1) as f64;
        let completion = t;
        let launched = completion * BUILD_SECONDS;
        let oy = (i * (tile_h + gap)) as f64;
        // Daytime outdoor block so the routine isn't asleep while he builds.
        let scene = scene_at(
            WIDTH,
            height(),
            BAND,
            10.0,
            SUNRISE,
            SUNSET,
            DAY,
            launched,
            40 + i as u64,
        );
        paint_tile(&mut sheet, 0.0, oy, &scene);
        let label = format!("BUILD {:02}%", (completion * 100.0).round() as u32);
        draw_label(&mut sheet, 4, oy as i32 + 4, &label, 2);
    }

    sheet.save(&out_dir.join("build.png"));
}

/// One stride, frame by frame — short-stage walk so the gait is readable.
pub fn walk_sheet(out_dir: &std::path::Path) {
    let frames = 12u32;
    let tile_w = (WIDTH * 0.45) as u32;
    let tile_h = height() as u32;
    let gap = 4u32;
    let mut sheet = PixmapInk::new(
        frames * (tile_w + gap) - gap,
        tile_h,
        STEEL_DEEP,
    );

    for i in 0..frames {
        let progress = i as f64 / frames as f64;
        let ox = (i * (tile_w + gap)) as f64;
        let scene = Scene {
            width: tile_w as f64,
            height: height(),
            band: BAND,
            doing: Doing::Walking,
            place: Place::Garden,
            previous: Place::Doorstep,
            progress,
            completion: 1.0,
            frame: i as u64,
            seconds: progress * crate::routine::WALK_SECONDS,
        };
        paint_tile(&mut sheet, ox, 0.0, &scene);
        let label = format!("WALK {:02}", i);
        draw_label(&mut sheet, ox as i32 + 4, 4, &label, 1);
    }

    sheet.save(&out_dir.join("walk.png"));
}

fn paint_tile(sheet: &mut PixmapInk, ox: f64, oy: f64, scene: &Scene) {
    let rail = BezPath::from_vec(
        Rect::new(0.0, scene.height - scene.band, scene.width, scene.height)
            .path_elements(0.25)
            .collect(),
    );
    let mut tile = OffsetInk {
        inner: sheet,
        dx: ox,
        dy: oy,
    };
    tile.fill(&rail, STEEL_BODY);
    // Deep backdrop above the rail for this tile only.
    let above = BezPath::from_vec(
        Rect::new(0.0, 0.0, scene.width, scene.height - scene.band)
            .path_elements(0.25)
            .collect(),
    );
    tile.fill(&above, STEEL_DEEP);
    f::paint(&mut tile, scene);
}
