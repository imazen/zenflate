struct Image {
    w: usize,
    h: usize,
    ch: usize,
    px: Vec<u8>, // row-major, w*h*ch bytes, 8-bit
}

/// Recursively collect `.png` paths under `dir` (some classes nest by sub-topic).
fn collect_pngs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_pngs(&p, out);
        } else if p.extension().is_some_and(|x| x == "png") {
            out.push(p);
        }
    }
}

fn decode_png(path: &Path) -> Option<Image> {
    let file = std::fs::File::open(path).ok()?;
    let dec = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None; // v1: 8-bit only
    }
    let ch = info.color_type.samples();
    buf.truncate(info.buffer_size());
    Some(Image {
        w: info.width as usize,
        h: info.height as usize,
        ch,
        px: buf,
    })
}

/// Center-crop to at most `cap` pixels, keeping local pixel statistics.
fn maybe_crop(img: &Image, cap: usize) -> Image {
    if img.w * img.h <= cap {
        return Image {
            w: img.w,
            h: img.h,
            ch: img.ch,
            px: img.px.clone(),
        };
    }
    // Largest square (in pixels) that fits under cap, bounded by the image.
    let side = (cap as f64).sqrt() as usize;
    let cw = side.min(img.w);
    let chh = side.min(img.h);
    let x0 = (img.w - cw) / 2;
    let y0 = (img.h - chh) / 2;
    let mut px = Vec::with_capacity(cw * chh * img.ch);
    for y in y0..y0 + chh {
        let row = &img.px[(y * img.w + x0) * img.ch..(y * img.w + x0 + cw) * img.ch];
        px.extend_from_slice(row);
    }
    Image {
        w: cw,
        h: chh,
        ch: img.ch,
        px,
    }
}

// ---------------------------------------------------------------------------
// Codec transforms
// ---------------------------------------------------------------------------

/// PNG adaptive filtering: per row pick the filter (None/Sub/Up/Average/Paeth)
/// with the minimum sum of absolute signed residuals (libpng's MSAD heuristic),
/// emit a filter-type byte then the filtered row.
fn t_png_filter(img: &Image) -> Vec<u8> {
    let (w, h, ch) = (img.w, img.h, img.ch);
    let rb = w * ch;
    let bpp = ch;
    let mut out = Vec::with_capacity(h * (rb + 1));
    let mut cand = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for c in &mut cand {
        c.resize(rb, 0u8);
    }
    let zero_row = vec![0u8; rb];
    for y in 0..h {
        let cur = &img.px[y * rb..y * rb + rb];
        let prev: &[u8] = if y == 0 {
            &zero_row
        } else {
            &img.px[(y - 1) * rb..(y - 1) * rb + rb]
        };
        for i in 0..rb {
            let a = if i >= bpp { cur[i - bpp] } else { 0 }; // left
            let b = prev[i]; // up
            let cc = if i >= bpp { prev[i - bpp] } else { 0 }; // up-left
            cand[0][i] = cur[i];
            cand[1][i] = cur[i].wrapping_sub(a);
            cand[2][i] = cur[i].wrapping_sub(b);
            cand[3][i] = cur[i].wrapping_sub(((a as u16 + b as u16) / 2) as u8);
            cand[4][i] = cur[i].wrapping_sub(paeth(a, b, cc));
        }
        // pick min sum-of-abs(signed)
        let mut best = 0usize;
        let mut best_cost = u64::MAX;
        for (f, row) in cand.iter().enumerate() {
            let cost: u64 = row.iter().map(|&v| (v as i8).unsigned_abs() as u64).sum();
            if cost < best_cost {
                best_cost = cost;
                best = f;
            }
        }
        out.push(best as u8);
        out.extend_from_slice(&cand[best]);
    }
    out
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (a as i16, b as i16, c as i16);
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

