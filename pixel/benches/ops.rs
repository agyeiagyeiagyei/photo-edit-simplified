//! Native micro-benchmarks for the pixel ops. Zero deps (no criterion);
//! medians of a few runs on a synthetic 6000×4000 RGBA buffer.
//!
//!   RUSTFLAGS="-C target-cpu=native" cargo bench -p pes-pixel
//!
//! Native numbers are a proxy for wasm (expect wasm 1.5–3× slower). Quote
//! them in PRs when the wasm path is unchanged. The RAW bench runs when a
//! fixture exists in `dl-raw/` (repo root) or `$RAW_FILE` is set.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use pes_pixel::curves::{CurvePoint, CurvesSettings};
use pes_pixel::levels::{LevelRange, LevelsSettings};
use pes_pixel::types::{EditParams, RawAdjust, Selection, SelectionKind};
use pes_pixel::{blur, denoise, geo, levels, ops, rawops};

const W: usize = 6000;
const H: usize = 4000;

fn synth(w: usize, h: usize) -> Vec<u8> {
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let s = ((x as f32 * 0.35 + y as f32 * 0.02).sin() * 0.5 + 0.5) as f32;
            px[i] = (60.0 + 140.0 * y as f32 / h as f32 * (0.6 + 0.4 * s)) as u8;
            px[i + 1] = (40.0 + 180.0 * x as f32 / w as f32) as u8;
            px[i + 2] = (90.0 * (0.5 + 0.5 * s)) as u8;
            px[i + 3] = 255;
        }
    }
    px
}

fn report(name: &str, mut samples: Vec<f64>) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = samples[samples.len() / 2];
    println!("{name:<38} {med:9.1} ms  (median of {})", samples.len());
}

/// Benchmark `f` that reads `base` and allocates its own result.
fn bench_read(name: &str, base: &[u8], iters: usize, mut f: impl FnMut(&[u8])) {
    f(base);
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        f(base);
        samples.push(t.elapsed().as_secs_f64() * 1e3);
    }
    report(name, samples);
}

/// Benchmark an in-place op; the restore copy is outside the timed region.
fn bench_mut(name: &str, base: &[u8], iters: usize, mut f: impl FnMut(&mut [u8])) {
    let mut buf = base.to_vec();
    f(&mut buf);
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        buf.copy_from_slice(base);
        let t = Instant::now();
        f(&mut buf);
        samples.push(t.elapsed().as_secs_f64() * 1e3);
    }
    report(name, samples);
}

fn levels_settings() -> LevelsSettings {
    let mut s = LevelsSettings::default();
    s.ranges[0] = LevelRange { black: 12.0, gamma: 1.2, white: 240.0, ..Default::default() };
    s
}

fn curves_settings() -> CurvesSettings {
    let mut s = CurvesSettings::default();
    s.channels[0] = vec![
        CurvePoint { x: 0.0, y: 0.0 },
        CurvePoint { x: 64.0, y: 50.0 },
        CurvePoint { x: 128.0, y: 150.0 },
        CurvePoint { x: 192.0, y: 210.0 },
        CurvePoint { x: 255.0, y: 255.0 },
    ];
    s
}

fn raw_fixture() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RAW_FILE") {
        return Some(PathBuf::from(p));
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("dl-raw");
    let entries = fs::read_dir(&dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        if ["arw", "dng", "nef", "cr2", "cr3", "raf", "orf", "rw2"].contains(&ext.as_str()) {
            return Some(p);
        }
    }
    None
}

fn main() {
    println!("synthetic {W}×{H} RGBA ({})", W * H);
    let base = synth(W, H);

    let rot = EditParams { rot90: 1, ..Default::default() };
    let fine = EditParams { fine_angle: 3.0, ..Default::default() };
    bench_read("geo::geometry rot90", &base, 3, |b| {
        let _ = geo::geometry(b, W, H, &rot);
    });
    bench_read("geo::geometry fine 3°", &base, 3, |b| {
        let _ = geo::geometry(b, W, H, &fine);
    });

    let stages: [(&str, RawAdjust); 8] = [
        ("wb", RawAdjust { temp: 7000.0, tint: 20.0, ..Default::default() }),
        ("exposure", RawAdjust { exposure: 0.5, ..Default::default() }),
        ("highlights", RawAdjust { highlights: 50.0, ..Default::default() }),
        ("shadows", RawAdjust { shadows: 50.0, ..Default::default() }),
        ("noise", RawAdjust { noise: 50.0, ..Default::default() }),
        ("denoise", RawAdjust { denoise: 50.0, ..Default::default() }),
        ("sharpen", RawAdjust { sharpen: 50.0, ..Default::default() }),
        (
            "all stages",
            RawAdjust {
                temp: 7000.0,
                tint: 20.0,
                exposure: 0.5,
                highlights: 50.0,
                shadows: 50.0,
                noise: 50.0,
                denoise: 50.0,
                sharpen: 50.0,
            },
        ),
    ];
    for (stage, adj) in &stages {
        let iters = if *stage == "denoise" || *stage == "all stages" { 2 } else { 5 };
        bench_mut(&format!("rawops::apply {stage}"), &base, iters, |b| {
            rawops::apply(b, W, H, adj);
        });
    }

    bench_mut("denoise::smart_denoise 0.5", &base, 2, |b| {
        denoise::smart_denoise(b, W, H, 0.5);
    });
    bench_mut("blur::blur_apply 50", &base, 5, |b| {
        blur::blur_apply(b, W, H, 50.0);
    });

    let tables = levels_settings().build_tables();
    bench_mut("levels::levels_apply", &base, 5, |b| {
        levels::levels_apply(b, &tables);
    });
    let curves = curves_settings();
    bench_mut("curves::apply", &base, 5, |b| {
        curves.apply(b);
    });
    bench_mut("ops::adjust", &base, 5, |b| {
        ops::adjust(b, 0.2, 0.2, 0.2, 0.2);
    });

    let lasso: Vec<(f32, f32)> = (0..16)
        .map(|i| {
            let a = i as f32 / 16.0 * std::f32::consts::TAU;
            (0.5 + 0.3 * a.cos(), 0.5 + 0.3 * a.sin())
        })
        .collect();
    let sel = Selection { kind: SelectionKind::Lasso(lasso), feather: 0.01 };
    bench_read("ops::selection_mask lasso+feather", &base, 3, |_| {
        let _ = ops::selection_mask(&sel, W, H);
    });

    match raw_fixture() {
        Some(path) => {
            let bytes = fs::read(&path).expect("read RAW fixture");
            println!("rawcore::decode_raw  {}", path.display());
            let mut samples = Vec::new();
            for _ in 0..2 {
                let t = Instant::now();
                let r = rawcore::decode_raw(&bytes);
                samples.push(t.elapsed().as_secs_f64() * 1e3);
                if let Err(e) = r {
                    println!("  decode failed: {e}");
                    return;
                }
            }
            report("rawcore::decode_raw", samples);
        }
        None => println!("rawcore::decode_raw  skipped (no fixture in dl-raw/, set RAW_FILE)"),
    }
}
