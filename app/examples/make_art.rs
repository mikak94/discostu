//! Regenerates the Disco Stu artwork in `assets/` from the vector source.
//!
//! `cargo run --example make_art [preview.png]`
//!
//! - `disco-stu.ai` (Illustrator 3, plain-text PostScript) → `disco-stu.svg`
//! - his head on a round lavender→pink badge → `stu-badge.svg`
//! - the badge rasterized → `icon.png` (window/taskbar) and `discostu.ico` (exe)

use std::fmt::Write as _;
use std::fs;

const ASSETS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");

/// Head region in SVG coordinates of `disco-stu.svg` (x, y, size).
const HEAD: (f32, f32, f32) = (22.0, -6.0, 234.0);

fn main() {
    let ai = fs::read_to_string(format!("{ASSETS}/disco-stu.ai")).expect("read disco-stu.ai");
    let (body, w, h) = convert(&ai);
    let full = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}">{body}</svg>"#
    );
    fs::write(format!("{ASSETS}/disco-stu.svg"), &full).expect("write svg");

    let (hx, hy, hs) = HEAD;
    let badge = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="256" height="256">
<defs>
<linearGradient id="suit" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="#b9b2f0"/><stop offset="1" stop-color="#f2a7d0"/>
</linearGradient>
<clipPath id="disc"><circle cx="128" cy="128" r="127"/></clipPath>
</defs>
<circle cx="128" cy="128" r="127" fill="url(#suit)"/>
<g clip-path="url(#disc)"><svg x="0" y="0" width="256" height="256" viewBox="{hx} {hy} {hs} {hs}">{body}</svg></g>
</svg>"##
    );
    fs::write(format!("{ASSETS}/stu-badge.svg"), &badge).expect("write badge");

    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let pngs: Vec<Vec<u8>> = sizes.iter().map(|&s| render(&badge, s, s)).collect();
    fs::write(format!("{ASSETS}/icon.png"), pngs.last().expect("256")).expect("write icon.png");
    fs::write(format!("{ASSETS}/discostu.ico"), ico(&sizes, &pngs)).expect("write ico");

    if let Some(preview) = std::env::args().nth(1) {
        let scale = 2;
        fs::write(&preview, render(&full, w as u32 * scale, h as u32 * scale)).expect("write preview");
    }
    println!("wrote disco-stu.svg ({w}×{h}), stu-badge.svg, icon.png, discostu.ico");
}

fn render(svg: &str, width: u32, height: u32) -> Vec<u8> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).expect("parse svg");
    let mut pixmap = tiny_skia::Pixmap::new(width, height).expect("pixmap");
    let size = tree.size();
    let t = tiny_skia::Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
    resvg::render(&tree, t, &mut pixmap.as_mut());
    pixmap.encode_png().expect("encode png")
}

/// .ico with PNG-compressed entries (Vista+).
fn ico(sizes: &[u32], pngs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, png) in sizes.iter().zip(pngs) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in pngs {
        out.extend_from_slice(png);
    }
    out
}

/// Illustrator 3 → SVG elements. Returns (elements, width, height).
fn convert(ai: &str) -> (String, f32, f32) {
    let bbox: Vec<f32> = ai
        .lines()
        .find_map(|l| l.strip_prefix("%%BoundingBox:"))
        .expect("bounding box")
        .split_whitespace()
        .map(|v| v.parse().expect("bbox number"))
        .collect();
    let (x0, y1) = (bbox[0], bbox[3]);
    let (w, h) = (bbox[2] - bbox[0], bbox[3] - bbox[1]);
    let pt = |x: f32, y: f32| format!("{:.2} {:.2}", x - x0, y1 - y);

    let mut out = String::new();
    let mut fill = [0f32; 4];
    let mut stroke = [0f32, 0.0, 0.0, 1.0];
    let mut width = 1.0f32;
    let mut evenodd = false;
    let mut d = String::new();
    let mut cur = (0f32, 0f32);
    // Compound path: subpaths gathered here, painted once at `*U`.
    let mut compound: Option<(String, Option<char>)> = None;

    let body = ai.split("%%EndSetup").nth(1).expect("setup section");
    for line in body.split(['\r', '\n']) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('%') {
            continue;
        }
        let mut parts: Vec<&str> = line.split_whitespace().collect();
        let op = parts.pop().expect("non-empty line");
        let n: Vec<f32> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        match op {
            "k" => fill = [n[0], n[1], n[2], n[3]],
            "K" => stroke = [n[0], n[1], n[2], n[3]],
            "g" => fill = [0.0, 0.0, 0.0, 1.0 - n[0]],
            "G" => stroke = [0.0, 0.0, 0.0, 1.0 - n[0]],
            "w" => width = n[0],
            "XR" => evenodd = n[0] == 1.0,
            "m" => {
                write!(d, "M{}", pt(n[0], n[1])).unwrap();
                cur = (n[0], n[1]);
            }
            "l" | "L" => {
                write!(d, "L{}", pt(n[0], n[1])).unwrap();
                cur = (n[0], n[1]);
            }
            "c" | "C" => {
                write!(d, "C{} {} {}", pt(n[0], n[1]), pt(n[2], n[3]), pt(n[4], n[5])).unwrap();
                cur = (n[4], n[5]);
            }
            "v" | "V" => {
                write!(d, "C{} {} {}", pt(cur.0, cur.1), pt(n[0], n[1]), pt(n[2], n[3])).unwrap();
                cur = (n[2], n[3]);
            }
            "y" | "Y" => {
                write!(d, "C{} {} {}", pt(n[0], n[1]), pt(n[2], n[3]), pt(n[2], n[3])).unwrap();
                cur = (n[2], n[3]);
            }
            "h" | "H" => d.push('Z'),
            "*u" => compound = Some((String::new(), None)),
            "*U" => {
                if let Some((cd, Some(paint))) = compound.take() {
                    emit(&mut out, &cd, paint, fill, stroke, width, true);
                }
            }
            "f" | "F" | "s" | "S" | "b" | "B" | "n" | "N" => {
                let paint = op.chars().next().expect("op char");
                if paint.is_lowercase() {
                    d.push('Z');
                }
                match &mut compound {
                    Some((cd, p)) => {
                        cd.push_str(&d);
                        p.get_or_insert(paint.to_ascii_lowercase());
                    }
                    None => emit(&mut out, &d, paint.to_ascii_lowercase(), fill, stroke, width, evenodd),
                }
                d.clear();
            }
            _ => {} // groups, overprint, dashes, joins: not needed
        }
    }
    (out, w, h)
}

fn emit(out: &mut String, d: &str, paint: char, fill: [f32; 4], stroke: [f32; 4], width: f32, evenodd: bool) {
    let rgb = |c: [f32; 4]| {
        let ch = |v: f32| ((1.0 - v) * (1.0 - c[3]) * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", ch(c[0]), ch(c[1]), ch(c[2]))
    };
    let f = if matches!(paint, 'f' | 'b') { rgb(fill) } else { "none".into() };
    let s = if matches!(paint, 's' | 'b') { rgb(stroke) } else { "none".into() };
    let rule = if evenodd { r#" fill-rule="evenodd""# } else { "" };
    writeln!(
        out,
        r#"<path d="{d}" fill="{f}" stroke="{s}" stroke-width="{width:.2}" stroke-linejoin="round" stroke-linecap="round"{rule}/>"#
    )
    .unwrap();
}
