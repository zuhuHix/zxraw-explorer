//! Lightweight image analysis for culling: sharpness estimate, perceptual hashes for
//! near-duplicate detection and burst grouping.
//!
//! These are *guidance only* heuristics, not quality judgements: a deliberately soft
//! portrait or a smooth sky will score low on sharpness.

use image::imageops::FilterType;
use image::{DynamicImage, GrayImage};
use serde::{Deserialize, Serialize};

/// Long edge the image is reduced to before analysis, so results are comparable across
/// resolutions (and fast).
pub const ANALYSIS_EDGE: u32 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sharpness {
    /// Variance of the 4-neighbour Laplacian on a ≤512 px grayscale copy (0..=255 scale).
    pub laplacian_variance: f64,
    /// `var / (var + 200)` mapped to 0..1 — monotonic, saturating; ~0.5 is "reasonably
    /// crisp" for typical photos at this scale. Guidance only.
    pub score: f32,
}

fn reduce_gray(img: &DynamicImage, edge: u32) -> GrayImage {
    let (w, h) = (img.width(), img.height());
    if w.max(h) > edge {
        img.thumbnail(edge, edge).to_luma8()
    } else {
        img.to_luma8()
    }
}

/// Sharpness estimate via variance of the Laplacian.
pub fn sharpness(img: &DynamicImage) -> Sharpness {
    let g = reduce_gray(img, ANALYSIS_EDGE);
    let var = laplacian_variance(&g);
    Sharpness {
        laplacian_variance: var,
        score: (var / (var + 200.0)) as f32,
    }
}

pub fn laplacian_variance(g: &GrayImage) -> f64 {
    let (w, h) = (g.width() as usize, g.height() as usize);
    if w < 3 || h < 3 {
        return 0.0;
    }
    let px = g.as_raw();
    let mut sum = 0f64;
    let mut sq = 0f64;
    let mut n = 0f64;
    for y in 1..h - 1 {
        let row = y * w;
        for x in 1..w - 1 {
            let c = px[row + x] as i32;
            let l = px[row + x - 1] as i32
                + px[row + x + 1] as i32
                + px[row - w + x] as i32
                + px[row + w + x] as i32
                - 4 * c;
            let l = l as f64;
            sum += l;
            sq += l * l;
            n += 1.0;
        }
    }
    let mean = sum / n;
    (sq / n - mean * mean).max(0.0)
}

/// 64-bit difference hash (9×8 grayscale, horizontal gradient signs).
pub fn dhash(img: &DynamicImage) -> u64 {
    let g = reduce_gray(img, 256);
    let s = image::imageops::resize(&g, 9, 8, FilterType::Triangle);
    let mut h = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let a = s.get_pixel(x, y)[0];
            let b = s.get_pixel(x + 1, y)[0];
            h = (h << 1) | u64::from(a > b);
        }
    }
    h
}

/// 64-bit average hash (8×8 grayscale vs mean).
pub fn ahash(img: &DynamicImage) -> u64 {
    let g = reduce_gray(img, 256);
    let s = image::imageops::resize(&g, 8, 8, FilterType::Triangle);
    let mean = s.pixels().map(|p| p[0] as u32).sum::<u32>() / 64;
    s.pixels()
        .fold(0u64, |h, p| (h << 1) | u64::from(p[0] as u32 > mean))
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 1.0 = identical hashes, 0.0 = every bit differs. ≥ ~0.85 (≤ 10 bits) usually means the
/// same picture resized/re-encoded.
pub fn hash_similarity(a: u64, b: u64) -> f32 {
    1.0 - hamming(a, b) as f32 / 64.0
}

/// Input for [`group_bursts`].
#[derive(Debug, Clone, PartialEq)]
pub struct BurstItem {
    /// Capture time in seconds (any consistent epoch; sub-seconds allowed).
    pub capture_time: f64,
    /// Camera identity (e.g. make + model + serial). Items with different cameras are never
    /// grouped together; `None` only groups with other `None`.
    pub camera: Option<String>,
}

/// Groups items into bursts: same camera, consecutive capture-time gaps ≤ `max_gap_secs`.
/// Returns groups of indices into `items` (each group sorted by time; groups of one
/// included, so every index appears exactly once). Items with non-finite times are returned
/// as singleton groups.
pub fn group_bursts(items: &[BurstItem], max_gap_secs: f64) -> Vec<Vec<usize>> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|&a, &b| {
        let (x, y) = (&items[a], &items[b]);
        x.camera
            .cmp(&y.camera)
            .then(x.capture_time.total_cmp(&y.capture_time))
    });
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in idx {
        let it = &items[i];
        let joins = it.capture_time.is_finite()
            && groups.last().and_then(|g| g.last()).is_some_and(|&p| {
                let prev = &items[p];
                prev.camera == it.camera
                    && prev.capture_time.is_finite()
                    && it.capture_time - prev.capture_time <= max_gap_secs
            });
        if joins {
            if let Some(g) = groups.last_mut() {
                g.push(i);
            }
        } else {
            groups.push(vec![i]);
        }
    }
    groups.sort_by(|a, b| {
        items[a[0]]
            .capture_time
            .total_cmp(&items[b[0]].capture_time)
    });
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(t: f64, cam: &str) -> BurstItem {
        BurstItem {
            capture_time: t,
            camera: Some(cam.into()),
        }
    }

    #[test]
    fn bursts_split_on_gap_and_camera() {
        let items = vec![
            item(0.0, "A"),
            item(0.5, "A"),
            item(0.9, "B"),
            item(1.0, "A"),
            item(10.0, "A"),
            item(f64::NAN, "A"),
        ];
        let g = group_bursts(&items, 1.0);
        assert!(g.contains(&vec![0, 1, 3]));
        assert!(g.contains(&vec![2]));
        assert!(g.contains(&vec![4]));
        assert!(g.contains(&vec![5]));
        assert_eq!(g.iter().map(Vec::len).sum::<usize>(), items.len());
        assert!(group_bursts(&[], 1.0).is_empty());
    }

    #[test]
    fn hashes_and_hamming() {
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hash_similarity(5, 5), 1.0);
        let tiny = DynamicImage::new_rgb8(1, 1);
        let _ = dhash(&tiny);
        let _ = ahash(&tiny);
        assert_eq!(sharpness(&tiny).laplacian_variance, 0.0);
    }
}
