//! Reading order of OCR text blocks by recursive XY-cut: vertical pages cut
//! tiers (top to bottom) first, then columns right to left; horizontal pages cut
//! columns left to right first, then bands top to bottom.

pub type R = [f32; 4];

fn split(rects: &[R], idx: &[usize], axis_y: bool, gap: f32) -> Vec<Vec<usize>> {
    let (s, e) = if axis_y { (1, 3) } else { (0, 2) };
    let mut v = idx.to_vec();
    v.sort_by(|&a, &b| rects[a][s].total_cmp(&rects[b][s]));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut end = f32::NEG_INFINITY;
    for i in v {
        if groups.is_empty() || rects[i][s] > end + gap {
            groups.push(vec![i]);
            end = rects[i][e];
        } else {
            groups.last_mut().unwrap().push(i);
            end = end.max(rects[i][e]);
        }
    }
    groups
}

pub fn order(rects: &[R], vertical: bool, gap: f32) -> Vec<usize> {
    let idx: Vec<usize> = (0..rects.len()).collect();
    let mut out = Vec::with_capacity(rects.len());
    cut(rects, &idx, vertical, gap, &mut out, 0);
    out
}

fn cut(rects: &[R], idx: &[usize], vertical: bool, gap: f32, out: &mut Vec<usize>, depth: u32) {
    if idx.len() <= 1 || depth > 64 {
        out.extend_from_slice(idx);
        return;
    }
    let cols = || {
        let mut g = split(rects, idx, false, gap);
        if vertical {
            g.reverse();
        }
        g
    };
    let rows = || split(rects, idx, true, gap);
    let (a, b) = if vertical { (rows(), cols()) } else { (cols(), rows()) };
    let groups = if a.len() > 1 { a } else { b };
    if groups.len() > 1 {
        for g in groups {
            cut(rects, &g, vertical, gap, out, depth + 1);
        }
        return;
    }
    let mut v = idx.to_vec();
    if vertical {
        v.sort_by(|&a, &b| rects[b][2].total_cmp(&rects[a][2]).then(rects[a][1].total_cmp(&rects[b][1])));
    } else {
        v.sort_by(|&a, &b| rects[a][1].total_cmp(&rects[b][1]).then(rects[a][0].total_cmp(&rects[b][0])));
    }
    out.extend(v);
}
