use anyhow::{Context, Result};
use image::{imageops::FilterType, DynamicImage, GrayImage, ImageReader};
use imageproc::edges::canny;
use serde::Serialize;
use std::path::Path;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize)]
pub struct VisionScore {
    pub edge_density: f64,
    pub normalized_variance: f64,
    pub bimodal_fraction: f64,
    pub horizontal_line_rows: usize,
    pub likely_document: bool,
}

pub fn inspect(path: &Path) -> Result<VisionScore> {
    let image = ImageReader::open(path)?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("decoding image {}", path.display()))?;
    Ok(score_image(image))
}

pub fn inspect_cancellable(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<Option<VisionScore>> {
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let image = ImageReader::open(path)?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("decoding image {}", path.display()))?;
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let score = score_image(image);
    Ok((!cancellation.is_cancelled()).then_some(score))
}

pub fn score_image(image: DynamicImage) -> VisionScore {
    let scaled = image.resize(500, 500, FilterType::Triangle).to_luma8();
    score_gray(&scaled)
}

fn score_gray(gray: &GrayImage) -> VisionScore {
    let total = (gray.width() as usize * gray.height() as usize).max(1);
    let mut sum = 0_f64;
    let mut squared = 0_f64;
    let mut dark = 0_usize;
    let mut bright = 0_usize;
    for pixel in gray.pixels() {
        let value = pixel[0] as usize;
        sum += value as f64;
        squared += (value * value) as f64;
        if value < 70 {
            dark += 1;
        }
        if value > 185 {
            bright += 1;
        }
    }
    let mean = sum / total as f64;
    let variance = ((squared / total as f64) - mean * mean).max(0.0) / (255.0 * 255.0);
    let bimodal_fraction = (dark + bright) as f64 / total as f64;
    let edges = canny(gray, 55.0, 125.0);
    let edge_count = edges.pixels().filter(|pixel| pixel[0] > 0).count();
    let edge_density = edge_count as f64 / total as f64;
    let horizontal_line_rows = (0..gray.height())
        .filter(|y| {
            let dark_count = (0..gray.width())
                .filter(|x| gray.get_pixel(*x, *y)[0] < 145)
                .count();
            dark_count as f64 / gray.width().max(1) as f64 > 0.32
        })
        .count();
    let line_ratio = horizontal_line_rows as f64 / gray.height().max(1) as f64;
    let likely_document = edge_density > 0.006
        && variance > 0.035
        && bimodal_fraction > 0.25
        && (line_ratio > 0.015 || edge_density > 0.018);
    VisionScore {
        edge_density,
        normalized_variance: variance,
        bimodal_fraction,
        horizontal_line_rows,
        likely_document,
    }
}
