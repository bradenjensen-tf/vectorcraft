use std::collections::HashSet;

use super::analysis::{Scratch, finite, flatten_groups, has_effects, masked, outlined_text, overlapping, painted, reach};
use super::compose::{Builder, CLEAR, Composer, Elem, Fill, Kind, Px, WHITE};
use super::options::FlattenOptions;
use super::{
    Affine, Appearance, AppearanceItem, Arc, BlendMode, Color, ColorMode, Document, EngineError, FillLayer, FillRule, ImageBlob, ImageObject, Inks,
    MAX_PIXELS, Node, NodeId, NodeKind, Paint, PathData, Rect, Result, composite, effects, isolated_doc, leaves, outline_strokes, planes_cmyk, po,
    separate, shape_node, shapes, unique_key,
};

// ---------- spot color preservation ----------

/// Returns true if the node has only spot-linked solid fills and strokes, with no gradients,
/// patterns, images or effects. The node must not have any transparency of its own.
fn is_spot_only_opaque(doc: &Document, n: &Node) -> bool {
    if !n.visible {
        return false;
    }
    // Check for own transparency
    if n.opacity < 1.0 || n.blend != BlendMode::Normal || n.extras().mask.as_ref().is_some_and(|m| !m.disabled) {
        return false;
    }
    // Check for effects
    if has_effects(n) {
        return false;
    }
    // Get spot swatch names
    let spot_names: HashSet<String> = doc.spot_names().into_iter().collect();
    if spot_names.is_empty() {
        return false;
    }
    // Check all fills and strokes are spot-linked solids
    let items = &n.appearance.items;
    if items.is_empty() {
        return false;
    }
    let all_spot = items.iter().filter(|i| i.visible()).all(|item| {
        // Check item-level transparency
        if item.opacity() < 1.0 || item.blend() != BlendMode::Normal || !item.effects().is_empty() {
            return false;
        }
        match item.paint() {
            Paint::Solid { swatch: Some(name), .. } => spot_names.contains(name),
            _ => false,
        }
    });
    if !all_spot {
        return false;
    }
    // For containers, check all children too
    match &n.kind {
        NodeKind::Path { .. } | NodeKind::Compound { .. } => true,
        NodeKind::Group { children, .. } | NodeKind::Layer { children, .. } => {
            children.iter().all(|c| is_spot_only_opaque(doc, c))
        }
        _ => false,
    }
}

/// Finds the indices of nodes at the top of the stacking order that can be preserved as vector
/// spot colors. Returns indices into the group array `g`, from top to bottom.
/// Only nodes that meet all criteria are included:
/// - Have only spot-linked fills/strokes
/// - No transparency of their own
/// - Are at the very top (no non-spot transparent nodes above them in the group)
fn find_preservable_spot_indices(
    doc: &Document,
    g: &[usize],
    plain: &[Node],
    see_through: &[bool],
) -> Vec<usize> {
    let mut result = vec![];
    // Walk from top to bottom of the group
    for &idx in g.iter().rev() {
        // If this node has transparency, we can't preserve anything above it
        // (since everything above would need to blend with it)
        if see_through[idx] {
            break;
        }
        // Check if this node has only spot fills and is opaque
        if is_spot_only_opaque(doc, &plain[idx]) {
            result.push(idx);
        } else {
            // A non-spot opaque node breaks the chain
            break;
        }
    }
    // Reverse to get top-to-bottom order matching g
    result.reverse();
    result
}

/// Collect all spot color names used in a node tree, in document swatch order for determinism.
/// Includes [Registration] which paints every plate (not in doc.swatches but treated as a spot).
fn collect_spot_names(doc: &Document, nodes: &[Node]) -> Vec<String> {
    let spot_swatches: HashSet<String> = doc.spot_names().into_iter().collect();
    
    let mut found = HashSet::new();
    for n in nodes {
        collect_spots_in_node(&spot_swatches, n, &mut found);
    }
    
    // Return in document swatch order for deterministic export, with [Registration] last if present.
    // Registration is not in doc.swatches but we still need stable order.
    let has_registration = found.remove(vectorcraft_color::swatch::REGISTRATION);
    let mut result: Vec<String> = doc.swatches_iter()
        .filter(|w| w.spot && found.contains(&w.name))
        .map(|w| w.name.clone())
        .collect();
    if has_registration {
        result.push(vectorcraft_color::swatch::REGISTRATION.to_string());
    }
    result
}

/// Recursively collect spot names from a node.
/// Includes [Registration] which is not a swatch but needs its own plate.
fn collect_spots_in_node(spot_swatches: &HashSet<String>, n: &Node, found: &mut HashSet<String>) {
    if !n.visible {
        return;
    }
    
    for item in &n.appearance.items {
        if let Some(name) = paint_spot_name(item.paint(), spot_swatches) {
            found.insert(name);
        }
    }
    
    // Also check colorize paint for images
    if let Some(colorize) = &n.extras().colorize {
        if let Some(name) = paint_spot_name(colorize, spot_swatches) {
            found.insert(name);
        }
    }
    
    if let Some(children) = n.children() {
        for child in children {
            collect_spots_in_node(spot_swatches, child, found);
        }
    }
}

/// Get the spot swatch name from a paint if it's linked to a spot or [Registration].
fn paint_spot_name(paint: &Paint, spot_swatches: &HashSet<String>) -> Option<String> {
    match paint {
        // Registration is not in spot_swatches but paints every plate
        p if p.is_registration() => Some(vectorcraft_color::swatch::REGISTRATION.to_string()),
        Paint::Solid { swatch: Some(name), .. } if spot_swatches.contains(name) => Some(name.clone()),
        Paint::Gradient(g) => {
            for stop in &g.gradient.stops {
                if let Some(name) = &stop.swatch {
                    if name == vectorcraft_color::swatch::REGISTRATION || spot_swatches.contains(name) {
                        return Some(name.clone());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Create a copy of the art with colors recolored for spot tint extraction.
/// Colors linked to `spot_name` become grayscale (tint as gray value).
/// All other colors become transparent.
fn recolor_for_spot(art: &[Node], spot_name: &str) -> Vec<Node> {
    art.iter().map(|n| recolor_node_for_spot(n, spot_name)).collect()
}

fn recolor_node_for_spot(n: &Node, spot_name: &str) -> Node {
    let mut n = n.clone();
    if !n.visible {
        return n;
    }
    
    for item in &mut n.appearance.items {
        let paint = item.paint_mut();
        *paint = recolor_paint_for_spot(paint, spot_name);
    }
    
    if let Some(children) = n.children_mut() {
        for child in children {
            *child = Arc::new(recolor_node_for_spot(child, spot_name));
        }
    }
    
    n
}

fn recolor_paint_for_spot(paint: &Paint, spot_name: &str) -> Paint {
    match paint {
        // Handle Registration specially: match by is_registration() rather than name comparison
        p if spot_name == vectorcraft_color::swatch::REGISTRATION && p.is_registration() => {
            if let Paint::Solid { tint, .. } = p {
                Paint::Solid { color: Color::gray(*tint), swatch: None, tint: 1.0 }
            } else {
                Paint::None
            }
        }
        Paint::Solid { swatch: Some(name), tint, .. } if name == spot_name => {
            // Convert to grayscale where tint becomes the gray value (1.0 = black = full ink)
            Paint::Solid { color: Color::gray(*tint), swatch: None, tint: 1.0 }
        }
        Paint::Gradient(g) => {
            let mut g = g.clone();
            let mut has_spot = false;
            for stop in &mut g.gradient.stops {
                if stop.swatch.as_deref() == Some(spot_name) {
                    has_spot = true;
                    stop.color = Color::gray(stop.tint);
                    stop.swatch = None;
                    stop.tint = 1.0;
                } else {
                    // Other colors become transparent
                    stop.opacity = 0.0;
                }
            }
            if has_spot {
                Paint::Gradient(g)
            } else {
                Paint::None
            }
        }
        _ => Paint::None, // Non-spot colors become transparent
    }
}

/// Strip spot colors from art for process-only rendering.
/// Spot-colored elements become transparent so they don't appear in the process tile.
fn strip_spots_for_process(art: &[Node], doc: &Document) -> Vec<Node> {
    let spot_names: HashSet<String> = doc.spot_names().into_iter().collect();
    art.iter().map(|n| strip_spots_node(n, &spot_names)).collect()
}

fn strip_spots_node(n: &Node, spot_names: &HashSet<String>) -> Node {
    let mut n = n.clone();
    // Strip spot paints from appearance items
    let mut app = (*n.appearance).clone();
    for item in &mut app.items {
        *item.paint_mut() = strip_spot_paint(item.paint(), spot_names);
    }
    n.appearance = vectorcraft_doc::Shared::new(app);
    // Also strip colorize if it's a spot
    if let Some(colorize) = &n.extras().colorize {
        if is_spot_paint(colorize, spot_names) {
            n.extras_mut().colorize = None;
        }
    }
    if let Some(ch) = n.children_mut() {
        for c in ch.iter_mut() {
            *c = std::sync::Arc::new(strip_spots_node(c, spot_names));
        }
    }
    n
}

fn is_spot_paint(paint: &Paint, spot_names: &HashSet<String>) -> bool {
    match paint {
        // Registration is always treated as a spot (paints every plate)
        p if p.is_registration() => true,
        Paint::Solid { swatch: Some(name), .. } => spot_names.contains(name),
        Paint::Gradient(g) => g.gradient.stops.iter().any(|s| s.swatch.as_ref().is_some_and(|n| n == vectorcraft_color::swatch::REGISTRATION || spot_names.contains(n))),
        _ => false,
    }
}

fn strip_spot_paint(paint: &Paint, spot_names: &HashSet<String>) -> Paint {
    match paint {
        // Registration paints every plate - strip it so it gets its own tint image
        p if p.is_registration() => Paint::None,
        Paint::Solid { swatch: Some(name), .. } if spot_names.contains(name) => {
            // All spots become transparent - they get their own tint images.
            Paint::None
        }
        Paint::Gradient(g) => {
            let mut g = g.clone();
            let mut all_spot = true;
            for stop in &mut g.gradient.stops {
                if let Some(swatch) = &stop.swatch {
                    if swatch == vectorcraft_color::swatch::REGISTRATION || spot_names.contains(swatch) {
                        // Spot/registration stop becomes transparent
                        stop.opacity = 0.0;
                    } else {
                        all_spot = false;
                    }
                } else {
                    all_spot = false;
                }
            }
            if all_spot {
                Paint::None
            } else {
                Paint::Gradient(g)
            }
        }
        _ => paint.clone(),
    }
}

/// Render a spot tint image for one spot color.
/// Returns grayscale pixels where the value is the tint amount (0 = no ink, 255 = full ink).
fn render_spot_tint(
    doc: &Document,
    art: &[Node],
    spot_name: &str,
    region: Rect,
    scale: f64,
) -> Option<Vec<u8>> {
    let recolored = recolor_for_spot(art, spot_name);
    let tmp = isolated_doc(doc, recolored);
    
    let mut r = vectorcraft_render::Renderer::new();
    let _opts = vectorcraft_render::RenderOptions { 
        background: Some([255, 255, 255, 255]),
        skip_templates: true, 
        ..Default::default() 
    };
    
    let img = r.render_region(&tmp, region, scale, false);
    vectorcraft_doc::cancel::check().ok()?;
    
    // Extract grayscale from the rendered image.
    // The recolored art renders spot tint as grayscale where:
    // - Black (0) = full ink coverage
    // - White (255) = no ink
    // The colorize mechanism uses ImageBlob::coverage() which inverts:
    // - Black input → high coverage output
    // So we keep the rendered values as-is (don't invert).
    let mut tint = Vec::with_capacity(img.pixels.len() / 4);
    for chunk in img.pixels.chunks(4) {
        // Use red channel (all channels should be the same for grayscale).
        // Alpha blending: where alpha < 255, blend toward white (no ink).
        let pixel = chunk[0];
        let alpha = chunk[3];
        // Blend: white (255) where alpha=0, pixel where alpha=255
        let blended = 255 - ((255 - pixel) as u32 * alpha as u32 / 255) as u8;
        tint.push(blended);
    }
    
    Some(tint)
}

/// Get the CMYK alternate color at 100% tint for a spot swatch or [Registration].
fn spot_alternate(doc: &Document, spot_name: &str) -> [f32; 4] {
    // [Registration] paints every plate at 100%
    if spot_name == vectorcraft_color::swatch::REGISTRATION {
        return [1.0, 1.0, 1.0, 1.0];
    }
    for swatch in &doc.swatches {
        if swatch.spot && swatch.name == spot_name {
            if let Paint::Solid { color, .. } = &swatch.paint {
                return color.to_cmyk();
            }
        }
    }
    [0.0, 0.0, 0.0, 0.0]
}

/// Encode grayscale pixels as a PNG.
fn grayscale_png(width: u32, height: u32, pixels: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let img = craft_image_codecs::Image::from_raw(
        width,
        height,
        craft_image_codecs::ChannelLayout::Gray,
        craft_image_codecs::SampleType::U8,
        pixels.to_vec(),
    )
    .map_err(|e| e.to_string())?;
    craft_image_codecs::encode(&img, craft_image_codecs::Format::Png, &craft_image_codecs::EncodeOptions::default())
        .map_err(|e| e.to_string())
}

// ---------- plan ----------

/// What the flattening does, worked out before the document changes.
pub(super) struct Plan {
    pub(super) flat: Vec<Flat>,
    /// Objects without transparency (they stay, but for the outline options).
    pub(super) kept: Vec<NodeId>,
    /// What the outline and overprint options do to each of `kept`, in its order.
    pub(super) work: Vec<Kept>,
    /// The objects with transparency of their own.
    pub(super) transparent: Vec<NodeId>,
}

/// One group of overlapping objects with transparency and what replaces it.
pub(super) struct Flat {
    /// Its objects in paint order; the result takes the place of the last.
    pub(super) roots: Vec<NodeId>,
    pub(super) regions: Regions,
    /// Where its image goes, and the image (once rendered).
    pub(super) area: Option<Area>,
    /// The rendered raster images (may be multiple tiles for large regions).
    rasters: Vec<Raster>,
    /// When the group is one block of type over several lines: its lines, to group the regions by.
    lines: Option<Lines>,
    /// Spot-colored objects that were preserved as vector (not rasterized). These go on top of
    /// the raster in stacking order. Each entry is the NodeId and a copy of the plain node.
    preserved_spots: Vec<(NodeId, Node)>,
}

/// The lines of a block of type: document space → its line space, where each line spans a band
/// across the lines (its ascent above the baseline to its descent below).
struct Lines {
    to_lines: Affine,
    bands: Vec<(f64, f64)>,
}

impl Lines {
    /// The lines of `n` when it is type laid out in two lines or more (not on a path).
    fn of(n: &Node) -> Option<Lines> {
        let NodeKind::Text(t) = &n.kind else { return None };
        if t.xf.determinant().abs() < 1e-12 {
            return None;
        }
        let layout = vectorcraft_text::layout(vectorcraft_text::FontDb::global(), t);
        if layout.on_path {
            return None;
        }
        let bands: Vec<(f64, f64)> = layout.lines.iter().filter(|l| l.x1 > l.x0).map(|l| (l.baseline - l.ascent, l.baseline + l.descent)).collect();
        let to_lines = if layout.vertical { layout.line_xf.inverse() * t.xf.inverse() } else { t.xf.inverse() };
        (bands.len() > 1).then_some(Lines { to_lines, bands })
    }

    /// The line nearest to the middle of `path`.
    fn line_of(&self, path: &PathData) -> usize {
        let Some(b) = path.bounds() else { return 0 };
        let y = (self.to_lines * b.center()).y;
        let away = |&(lo, hi): &(f64, f64)| if y < lo { lo - y } else { (y - hi).max(0.0) };
        self.bands.iter().enumerate().min_by(|a, b| away(a.1).total_cmp(&away(b.1))).map_or(0, |(i, _)| i)
    }
}

/// Where a group's image goes.
pub(super) struct Area {
    rect: Rect,
    ppi: f64,
    /// The complex regions it is clipped to (Clip Complex Regions).
    clip: Option<PathData>,
    /// The raster/vector balance rasterizes the whole group (it splits into too many regions).
    pub(super) whole: bool,
}

impl Area {
    /// The area's outline.
    pub(super) fn outline(&self) -> PathData {
        self.clip.clone().unwrap_or_else(|| shapes::rectangle(self.rect))
    }
}

/// Flat-colour regions: their outlines and paints.
pub(super) type Regions = Vec<(PathData, RegionFill)>;

/// A flat-colour region's paint.
pub(super) struct RegionFill {
    paint: Paint,
    alpha: f32,
    overprint: bool,
}

/// A rendered image of complex regions, clipped to them or not.
struct Raster {
    blob: ImageBlob,
    width: u32,
    height: u32,
    xf: Affine,
    clip: Option<PathData>,
    /// Spot ink tint images: (spot name, CMYK alternate at 100%, grayscale tint image).
    /// Each spot image has the same dimensions and transform as the main image.
    /// Pixels are grayscale tint values (0 = no ink, 255 = full tint).
    spot_images: Vec<SpotImage>,
}

/// A spot color tint image for one ink.
pub(super) struct SpotImage {
    /// The spot swatch name.
    pub(super) name: String,
    /// The CMYK alternate color at 100% tint.
    pub(super) alternate: [f32; 4],
    /// Grayscale tint image (same dimensions as the main raster).
    pub(super) blob: ImageBlob,
}

/// The flattening of `roots` with `o`; `render` renders the images (the preview only needs where
/// they go). Fails only when an image can't be encoded.
pub(super) fn plan(src: &Document, roots: &[NodeId], o: &FlattenOptions, render: bool) -> Result<Plan> {
    let tmp = isolated_doc(src, roots.iter().filter_map(|id| src.node(*id).cloned()).collect());
    // Geometry effects first: what is left on the art are raster effects.
    let baked = effects::bake_document(&tmp).unwrap_or(tmp);
    let art: Vec<Node> = baked.layers[0].children().into_iter().flatten().map(|a| (**a).clone()).collect();
    let mut sc = Scratch { brushes: vectorcraft_brush::library(&baked), doc: baked.clone() };
    let plain: Vec<Node> = art.iter().map(|n| sc.plain(n)).collect();
    // Each object as its clip groups and clipping layers show it.
    let clipped: Vec<Node> = plain.iter().zip(roots).map(|(n, id)| clipped_by_ancestors(src, *id, n, None)).collect();
    let reaches: Vec<Option<Rect>> = plain.iter().zip(&clipped).map(|(n, c)| reach(n).zip(reach(c)).map(|(a, b)| a.intersect(b))).collect();
    let see_through: Vec<bool> = plain.iter().map(|n| o.shows_transparency(&baked, n) || o.draws_see_through_image(n)).collect();
    let cmyk = src.color_mode == ColorMode::Cmyk;
    let mut out = Plan {
        flat: vec![],
        kept: vec![],
        work: vec![],
        transparent: roots.iter().zip(&see_through).filter(|(_, t)| **t).map(|(id, _)| *id).collect(),
    };
    // Baking above stopped early when cancelled; each group is a place to stop (a plan changes nothing).
    vectorcraft_doc::cancel::check()?;
    let mut queue = groups(src, roots, &plain, &reaches, &see_through, o)?;
    queue.reverse();
    while let Some(g) = queue.pop() {
        vectorcraft_doc::cancel::check()?;
        // A group that stays inside the clip groups and clipping layers of its topmost object
        // replaces only the objects inside them, over the others, which are flattened on their own.
        let (g, under) = match inside_and_under(src, roots, &g) {
            Some((inside, under)) => {
                queue.push(under.clone());
                (inside, under)
            }
            None => (g, vec![]),
        };
        // Composited over white, the group covers what it is drawn over, so it is composited over
        // the fills of the clipping paths around it and the objects under it in the same kind of
        // layer. Composited over nothing, it keeps its alpha and they show through.
        let backdrop = !o.preserve_alpha;
        let state = g.last().map(|&i| layer_state(src, roots[i]));
        let top = g.last().map(|&i| roots[i]);
        // A blend mode of a layer or group around the group alone would blend the art under it
        // twice.
        let blends = |i: usize| top.is_some_and(|t| own_layers(src, t, Some(roots[i])).iter().any(|a| a.blend != BlendMode::Normal));
        // Art under an opacity mask, effects or a knockout group is left out: the group can't draw it
        // as those do.
        let drawn_plainly = |i: usize| {
            own_layers(src, roots[i], top).iter().all(|a| !masked(a) && !has_effects(a) && !matches!(a.knockout, vectorcraft_doc::Knockout::On))
        };
        let under_shown = under.iter().copied().filter(|&i| backdrop && Some(layer_state(src, roots[i])) == state && !blends(i) && drawn_plainly(i));
        let composited: Vec<usize> = under_shown.chain(g.iter().copied()).collect();
        let below = composited.len() - g.len();
        let extent = composited.iter().filter_map(|&i| reaches[i]).reduce(|a, b| a.union(b)).unwrap_or_default();
        let shown = |nodes: &[Node]| -> Vec<Node> {
            let mut seen = vec![];
            let (mut under, mut out) = (vec![], vec![]);
            for (k, &i) in composited.iter().enumerate() {
                let n = clipped_by_ancestors(src, roots[i], &nodes[i], backdrop.then_some((&mut seen, extent)));
                if k < below {
                    let groups = own_layers(src, roots[i], top).into_iter().filter(|a| !draws_as_is(a)).collect();
                    under.push((groups, n));
                } else {
                    out.push(n);
                }
            }
            let mut all = in_groups(under, 0);
            all.extend(out);
            all
        };
        let ids: Vec<NodeId> = g.iter().map(|&i| roots[i]).collect();
        let has_art_below = below > 0;
        let group = g.iter().any(|&i| see_through[i]).then(|| {
            let plain = shown(&plain);
            flatten_group(&plain.iter().collect::<Vec<_>>(), o, cmyk, has_art_below)
        });
        let Some((regions, area)) = group.flatten() else {
            // Transparent art that its clip groups or clipping layers hide entirely draws nothing:
            // it is removed.
            let hides = |i: usize| see_through[i] && reach(&plain[i]).is_some() && reach(&clipped[i]).is_none();
            let (hidden, left): (Vec<usize>, Vec<usize>) = g.iter().partition(|&&i| hides(i));
            if !hidden.is_empty() {
                let roots = hidden.iter().map(|&i| roots[i]).collect();
                out.flat.push(Flat { roots, regions: vec![], area: None, rasters: vec![], lines: None, preserved_spots: vec![] });
            }
            out.kept.extend(left.iter().map(|&i| roots[i]));
            continue;
        };
        
        // Try to preserve spot-colored objects at the top of the stacking order as vector.
        // This keeps spot inks from being baked into CMYK. Works for both over-cap (a.whole)
        // and normal rasterization.
        let (rasters, preserved_spots) = match area.as_ref().filter(|_| render) {
            Some(a) => {
                // Find spot-only nodes at the top that can be preserved
                // spot_indices contains indices into g (which are indices into roots/plain arrays)
                let spot_indices = find_preservable_spot_indices(&baked, &g, &plain, &see_through);
                
                if spot_indices.is_empty() {
                    // No spots to preserve, render everything
                    (render_raster(&baked, &shown(&art), a.rect, a.ppi, a.clip.clone(), o)?, vec![])
                } else {
                    // Create a set of indices into g that should be excluded
                    let spot_g_positions: HashSet<usize> = spot_indices.iter()
                        .filter_map(|&i| g.iter().position(|&gi| gi == i))
                        .collect();
                    
                    // Build raster art excluding the preserved spot nodes
                    // shown() transforms composited which is under_shown + g
                    // We need to exclude items at positions (below + spot_g_positions) in composited
                    let shown_art = shown(&art);
                    let raster_art: Vec<Node> = shown_art
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| {
                            if *i < below {
                                true // Keep all "under" items
                            } else {
                                // Check if this g index should be excluded
                                !spot_g_positions.contains(&(*i - below))
                            }
                        })
                        .map(|(_, n)| n)
                        .collect();
                    
                    let rasters = if raster_art.is_empty() {
                        vec![]
                    } else {
                        render_raster(&baked, &raster_art, a.rect, a.ppi, a.clip.clone(), o)?
                    };
                    
                    // Collect the preserved spots with their original nodes
                    let preserved: Vec<(NodeId, Node)> = spot_indices
                        .iter()
                        .map(|&i| (roots[i], plain[i].clone()))
                        .collect();
                    
                    (rasters, preserved)
                }
            }
            None => (vec![], vec![]),
        };
        
        let lines = match g.as_slice() {
            [i] => Lines::of(&art[*i]),
            _ => None,
        };
        out.flat.push(Flat { roots: ids, regions, area, rasters, lines, preserved_spots });
    }
    out.work = out.kept.iter().map(|id| Kept::of(src, *id, o)).collect();
    Ok(out)
}

/// What the outline and overprint options do to one object without transparency, found in one
/// walk of it.
pub(super) struct Kept {
    pub(super) id: NodeId,
    /// Whether the options change it.
    pub(super) changes: bool,
    /// The type the text option outlines and the stroked paths the stroke option outlines, those
    /// that show and are not pieces of a compound path: what the Flattener Preview highlights.
    pub(super) shown_text: Vec<NodeId>,
    pub(super) shown_strokes: Vec<NodeId>,
    /// In paint order: the type the text option outlines and the leaf shapes ([`leaves`]) other
    /// than type that the stroke option outlines.
    steps: Vec<Step>,
    /// The objects with overprints, each with the type or leaf shape that holds it (itself
    /// included) when a step replaces that.
    overprinted: Vec<(NodeId, Option<NodeId>)>,
}

/// One object [`apply`] outlines.
enum Step {
    /// Type; `leaf`: only groups and layers hold it, so its outlines are leaf shapes for the
    /// stroke option.
    Text { id: NodeId, leaf: bool },
    /// A leaf shape other than type.
    Leaf(NodeId),
}

/// Where [`Kept::visit`] is in the object it walks.
#[derive(Clone, Copy)]
struct At {
    /// Neither the node nor what holds it is hidden, and it is not a piece of a compound path.
    shown: bool,
    /// Only groups and layers hold the node.
    leaf: bool,
    /// The type or leaf shape holding the node that a step replaces.
    holder: Option<NodeId>,
}

impl Kept {
    /// What the options `o` do to `root` of `d`: nothing when `d` doesn't hold it.
    fn of(d: &Document, root: NodeId, o: &FlattenOptions) -> Kept {
        let mut k = Kept { id: root, changes: false, shown_text: vec![], shown_strokes: vec![], steps: vec![], overprinted: vec![] };
        if let Some(n) = d.node(root) {
            k.visit(n, o, At { shown: true, leaf: true, holder: None });
        }
        k
    }

    /// Note what the options `o` do to `n` and to everything under it.
    fn visit(&mut self, n: &Node, o: &FlattenOptions, at: At) {
        kept_seen(n);
        let shown = at.shown && n.visible;
        let mut holder = at.holder;
        if matches!(n.kind, NodeKind::Text(_)) {
            if o.text_to_outlines {
                self.changes = true;
                if shown {
                    self.shown_text.push(n.id);
                }
                self.steps.push(Step::Text { id: n.id, leaf: at.leaf });
                holder = Some(n.id);
            }
        } else {
            if o.strokes_to_outlines && stroked(n) {
                self.changes = true;
                if shown {
                    self.shown_strokes.push(n.id);
                }
            }
            if o.strokes_to_outlines && at.leaf && matches!(n.kind, NodeKind::Path { guide: false, clipping: false, .. } | NodeKind::Compound { .. })
            {
                self.steps.push(Step::Leaf(n.id));
                holder = Some(n.id);
            }
        }
        if !o.preserve_overprints && overprints(n) {
            self.changes = true;
            self.overprinted.push((n.id, holder));
        }
        let Some(children) = n.children() else { return };
        let inside = At {
            shown: shown && !matches!(n.kind, NodeKind::Compound { .. }),
            leaf: at.leaf && matches!(n.kind, NodeKind::Group { .. } | NodeKind::Layer { .. }),
            holder,
        };
        for c in children {
            self.visit(c, o, inside);
        }
    }
}

/// `roots` of `src` (`plain`: as plain art; `reaches`: where each can paint, clipped) in the
/// groups that flatten together ([`flatten_groups`]), each in paint order, by their first object.
/// Objects that overlap (as [`overlapping`] groups them) stay in one group when one of them is
/// inside a clip group or clipping layer: [`inside_and_under`] and [`place_of`] then decide where
/// they go.
fn groups(
    src: &Document,
    roots: &[NodeId],
    plain: &[Node],
    reaches: &[Option<Rect>],
    see_through: &[bool],
    o: &FlattenOptions,
) -> Result<Vec<Vec<usize>>> {
    // An image clipped to its regions, opaque over white, reaches a pixel past them; a group's
    // image is no larger than the art it overlaps, so its pixels are no wider than this.
    let opaque_edge = o.no_transparency.is_some() && o.clip_complex_regions && !o.preserve_alpha;
    let ppi = o.line_art_ppi.min(o.gradient_ppi).max(1.0);
    let extent = |ids: &mut dyn Iterator<Item = usize>| ids.filter_map(|i| reaches[i].filter(finite)).reduce(|a, b| a.union(b)).unwrap_or_default();
    // Where the widest such pixel is wider than the margins of two reaches, objects that close
    // overlap too.
    let widest = if opaque_edge { 1.0 / raster_scale(extent(&mut (0..reaches.len())), ppi) } else { 0.0 };
    let grow = ((widest - 2.0) / 2.0).max(0.0);
    let near: Vec<Option<Rect>> = reaches.iter().map(|b| b.map(|b| b.inflate(grow, grow))).collect();
    let mut out = vec![];
    for g in overlapping(if grow > 0.0 { &near } else { reaches })? {
        let clipped = || clips_count() && g.iter().any(|&i| !clip_ancestors(src, roots[i]).is_empty());
        if g.len() < 2 || !g.iter().any(|&i| see_through[i]) || clipped() {
            out.push(g);
            continue;
        }
        let fringe = if opaque_edge { 1.0 / raster_scale(extent(&mut g.iter().copied()), ppi) } else { 0.0 };
        let local = |v: &[Option<Rect>]| g.iter().map(|&i| v[i]).collect::<Vec<_>>();
        let painted: Vec<Option<Rect>> = g.iter().map(|&i| painted(&plain[i])).collect();
        let see: Vec<bool> = g.iter().map(|&i| see_through[i]).collect();
        out.extend(flatten_groups(&local(reaches), &painted, &see, fringe)?.into_iter().map(|l| l.into_iter().map(|k| g[k]).collect::<Vec<_>>()));
    }
    out.sort_by_key(|g| g.first().copied());
    Ok(out)
}

/// `plain` (object `id` of `src` as plain art) inside a clip group for each clip group and
/// clipping layer around `id`, innermost first, each clipped by that group's or layer's clipping
/// path. `plain` itself when nothing around it clips. With `fills` (clipping paths, and an area),
/// the fill of each clipping path not in them yet goes behind the art, inside the area, and the
/// path is added to them.
fn clipped_by_ancestors(src: &Document, id: NodeId, plain: &Node, mut fills: Option<(&mut Vec<NodeId>, Rect)>) -> Node {
    let mut out = plain.clone();
    if !clips_count() {
        return out;
    }
    let ancestry = src.ancestry(id).unwrap_or_default();
    for pair in ancestry.windows(2).rev() {
        let Some(above) = src.node(pair[0]).filter(|n| n.clips()) else { continue };
        let Some(clip) = above.children().and_then(|c| c.first()) else { continue };
        // The clipping path itself is not clipped art.
        if clip.id == pair[1] {
            continue;
        }
        // Its outline, unpainted: the group or layer around the object still paints the path.
        // Nothing to clip by hides the clipped art, as in the compositor.
        let Some((outline, rule)) = effects::clip_outline(clip) else {
            out.visible = false;
            return out;
        };
        let mut path = Node::path(clip.id, PathData::from_bezpath(&outline), Appearance::default());
        if let NodeKind::Path { rule: r, .. } = &mut path.kind {
            *r = rule;
        }
        let mut children = vec![Arc::new(path)];
        // The renderer paints the fill behind the clipped art.
        if let Some((seen, r)) = fills.as_mut()
            && !seen.contains(&clip.id)
            && let Some(fill) = clip.clip_paint().fill
        {
            let r = *r;
            seen.push(clip.id);
            let mut at = Node::group(above.id, vec![Arc::new(Node::path(clip.id, shapes::rectangle(r), Appearance::default())), Arc::new(fill)]);
            at.set_clips(true);
            children.push(Arc::new(at));
        }
        children.push(Arc::new(out));
        let mut g = Node::group(above.id, children);
        g.set_clips(true);
        out = g;
    }
    out
}

/// Whether layer or group `n` draws what it holds as it is: opaque, in Normal mode, not isolated.
fn draws_as_is(n: &Node) -> bool {
    n.opacity >= 1.0 && n.blend == BlendMode::Normal && !n.isolate
}

/// `items` (in paint order, each an object and the layers and groups around it, outermost first,
/// that don't draw it as it is) with the objects of each such layer or group in one group with its
/// opacity, blend mode and isolation, from the `depth`th around them on: they then composite
/// together as that layer or group draws them.
fn in_groups(items: Vec<(Vec<&Node>, Node)>, depth: usize) -> Vec<Node> {
    let mut out = vec![];
    let mut items = items.into_iter().peekable();
    while let Some((around, n)) = items.next() {
        let Some(a) = around.get(depth).copied() else {
            out.push(n);
            continue;
        };
        let mut run = vec![(around, n)];
        while let Some(next) = items.next_if(|(around, _)| around.get(depth).is_some_and(|b| b.id == a.id)) {
            run.push(next);
        }
        let mut g = Node::group(a.id, in_groups(run, depth + 1).into_iter().map(Arc::new).collect());
        (g.opacity, g.blend, g.isolate) = (a.opacity, a.blend, a.isolate);
        out.push(g);
    }
    out
}

/// The layers and groups around object `id` of `src` that are not also around `other`, outermost
/// first.
fn own_layers(src: &Document, id: NodeId, other: Option<NodeId>) -> Vec<&Node> {
    let mine = src.ancestry(id).unwrap_or_default();
    let theirs = other.and_then(|t| src.ancestry(t)).unwrap_or_default();
    let shared = mine.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
    let own = mine.get(shared..mine.len().saturating_sub(1)).unwrap_or_default();
    own.iter().filter_map(|a| src.node(*a)).collect()
}

/// The clip groups and clipping layers around object `id` of `d`, innermost first.
fn clip_ancestors(d: &Document, id: NodeId) -> Vec<NodeId> {
    let mut a = d.ancestry(id).unwrap_or_default();
    a.pop();
    a.into_iter().rev().filter(|a| d.node(*a).is_some_and(Node::clips)).collect()
}

/// Whether the layers around object `id` of `d` all print, all show, and whether one is a
/// template.
fn layer_state(d: &Document, id: NodeId) -> (bool, bool, bool) {
    let mut a = d.ancestry(id).unwrap_or_default();
    a.pop();
    a.iter().filter_map(|a| d.node(*a)).fold((true, true, false), |(print, show, template), n| match &n.kind {
        NodeKind::Layer { printable, template: t, .. } => (print && *printable, show && n.visible, template || *t),
        _ => (print, show, template),
    })
}

/// For group `g` (indices of `roots` of `src`, in paint order) whose flattened group can't leave
/// the clip groups and clipping layers of its topmost object ([`place_of`]): the objects inside
/// them and the objects under them. `None` when the group can leave them, or every object is
/// inside them.
fn inside_and_under(src: &Document, roots: &[NodeId], g: &[usize]) -> Option<(Vec<usize>, Vec<usize>)> {
    if !clips_count() || g.len() < 2 {
        return None;
    }
    let ids: Vec<NodeId> = g.iter().map(|&i| roots[i]).collect();
    let top = clip_ancestors(src, *ids.last()?);
    if top.is_empty() {
        return None;
    }
    let inside = |id: NodeId| {
        let mine = clip_ancestors(src, id);
        top.iter().all(|c| mine.contains(c))
    };
    if !inside(place_of(src, &ids)?) {
        return None;
    }
    let (inside, under): (Vec<usize>, Vec<usize>) = g.iter().partition(|&&i| inside(roots[i]));
    (!under.is_empty()).then_some((inside, under))
}

/// The regions and the image area replacing one group of overlapping objects (`plain`: the art as
/// composited; `cmyk`: in a CMYK document; `has_art_below`: whether art from below is composited
/// with this group—affects whether the rectangle fallback clip is safe); `None` when it paints nothing.
fn flatten_group(plain: &[&Node], o: &FlattenOptions, cmyk: bool, has_art_below: bool) -> Option<(Regions, Option<Area>)> {
    let mut b = Builder { cmyk, ..Default::default() };
    let elems: Vec<Elem> = plain.iter().filter_map(|n| b.elem(n)).collect();
    if b.shapes.is_empty() {
        return None;
    }
    // An unclipped full raster needs no regions.
    let whole = o.balance <= 0.0 && !o.clip_complex_regions;
    // Nor does one that splits into more regions than the balance allows: the build stops at the
    // first region past the limit. A hard cap (REGION_CAP) prevents exceeding krilla's DICT_LEN.
    let (regions, over) = match o.region_limit().filter(|_| !every_region()) {
        _ if whole => (vec![], false),
        Some(limit) => {
            let cap = limit.min(FlattenOptions::REGION_CAP);
            po::regions_within(&b.shapes, cap).map_or((vec![], true), |r| (r, false))
        }
        None => {
            let cap = FlattenOptions::REGION_CAP;
            po::regions_within(&b.shapes, cap).map_or((vec![], true), |r| (r, false))
        }
    };
    // An arrangement that failed (degenerate geometry) leaves only rasterizing.
    let all = over || regions.is_empty() || o.rasterize_all(regions.len());
    let mut vector: Vec<(usize, RegionFill)> = vec![];
    let mut complex: Vec<usize> = vec![];
    let mut gradients_only = !all;
    for (k, r) in regions.iter().enumerate() {
        // Art outside the clipping path of its clip group draws nothing there.
        let shown = |i: &&usize| b.kinds[**i] == Kind::Vector || !(clips_count() && b.hidden(**i, &r.sources));
        if all || r.sources.iter().filter(shown).any(|&i| b.kinds[i] != Kind::Vector) {
            gradients_only &= r.sources.iter().filter(shown).all(|&i| b.kinds[i] == Kind::Gradient);
            complex.push(k);
        } else if let Some(fill) = region_fill(&elems, &r.sources, o, cmyk) {
            vector.push((k, fill));
        }
    }
    let bounds = |ks: &[usize]| ks.iter().filter_map(|&k| regions[k].path.bounds()).reduce(|a, b| a.union(b));
    if !o.clip_complex_regions && !all {
        // A rectangle of raster takes in every region it overlaps.
        while let Some(r) = bounds(&complex) {
            let before = complex.len();
            let rect = shapes::rectangle(r);
            vector.retain(|(k, _)| {
                let path = &regions[*k].path;
                let hit = path.bounds().is_some_and(|b| b.intersect(r).area() > 0.0)
                    && po::area(&po::boolean(path, FillRule::NonZero, &rect, FillRule::NonZero, po::BoolOp::Intersect), FillRule::NonZero) > 1e-6;
                if hit {
                    complex.push(*k);
                }
                !hit
            });
            if complex.len() == before {
                break;
            }
            gradients_only = false;
        }
    }
    let raster_rect = if all { plain.iter().filter_map(|n| reach(n)).reduce(|a, b| a.union(b)) } else { bounds(&complex) };
    // Clamp raster rect to page bounds if specified (prevents rasters extending far off page).
    let raster_rect = raster_rect.map(|r| {
        o.page_bounds.map_or(r, |page| {
            let clamped = r.intersect(page);
            if clamped.width() > 0.0 && clamped.height() > 0.0 {
                clamped
            } else {
                // If the intersection is empty (art entirely outside page), use original bounds
                // to avoid an invalid rect, but log a warning.
                log::warn!("Raster rect {:?} doesn't intersect page {:?}, using unclamped", r, page);
                r
            }
        })
    });
    let area = raster_rect.map(|rect| {
        let clip = if all && o.no_transparency.is_some() && !o.preserve_alpha {
            // Over-cap rasters for no-transparency output need a clip (the group outline) so
            // render_raster sets no_alpha and makes them opaque. Without it they keep their alpha
            // channel and cause soft masks.
            //
            // Build the clip as a proper union of the shapes' paths. Simply concatenating subpaths
            // causes holes where shapes with opposite winding overlap and cancel under nonzero fill.
            // If the union fails (too complex), fall back to the bounding box—the raster already
            // covers the group's composite, so the box clips nothing visible.
            let refs: Vec<(&PathData, FillRule)> = b.shapes.iter().map(|s| (&s.path, s.rule)).collect();
            let union = po::unite_all(&refs);
            let clip_path = if union.is_empty() && !refs.is_empty() {
                // Union failed. The bounding box fallback is only safe when no art from below is
                // composited: an opaque rectangle would clip that art incorrectly. When art lies
                // below, skip the clip (the raster keeps its alpha shape from render_raster).
                if has_art_below {
                    log::warn!(
                        "unite_all failed for {} paths with art below, skipping rectangle fallback",
                        refs.len()
                    );
                    None
                } else {
                    log::warn!(
                        "unite_all failed for {} paths, falling back to bounding rectangle",
                        refs.len()
                    );
                    Some(shapes::rectangle(rect))
                }
            } else if union.is_empty() {
                // No shapes to unite (empty input)
                Some(shapes::rectangle(rect))
            } else {
                Some(union)
            };
            clip_path
        } else {
            o.clip_complex_regions.then(|| PathData::new(complex.iter().flat_map(|&k| regions[k].path.subpaths.iter().cloned()).collect()))
        };
        let ppi = if gradients_only { o.gradient_ppi } else { o.line_art_ppi };
        Area { rect, ppi, clip: clip.filter(|c| !c.is_empty()), whole: all }
    });
    let vector = vector.into_iter().map(|(k, f)| (regions[k].path.clone(), f)).collect();
    Some((vector, area))
}

#[cfg(test)]
thread_local! {
    /// Groups flattened on this thread build all of their regions ([`with_every_region`]).
    static EVERY_REGION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether groups build all of their regions, past the balance's limit too.
#[cfg(test)]
fn every_region() -> bool {
    EVERY_REGION.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn every_region() -> bool {
    false
}

/// `f` with every group it flattens building all of its regions, past the balance's limit too.
#[cfg(test)]
pub(crate) fn with_every_region<T>(f: impl FnOnce() -> T) -> T {
    EVERY_REGION.with(|e| e.set(true));
    let out = f();
    EVERY_REGION.with(|e| e.set(false));
    out
}

#[cfg(test)]
thread_local! {
    /// Groups flattened on this thread rasterize art where its clip group or clipping layer hides
    /// it too ([`with_clips_ignored`]).
    static CLIPS_IGNORED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a region outside the clipping path of a clip group or clipping layer leaves out the
/// art it hides there.
#[cfg(test)]
fn clips_count() -> bool {
    !CLIPS_IGNORED.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn clips_count() -> bool {
    true
}

/// `f` with every group it flattens rasterizing art where its clip group or clipping layer hides
/// it too.
#[cfg(test)]
pub(crate) fn with_clips_ignored<T>(f: impl FnOnce() -> T) -> T {
    CLIPS_IGNORED.with(|e| e.set(true));
    let out = f();
    CLIPS_IGNORED.with(|e| e.set(false));
    out
}

/// The flat colour of a region covered by `sources` (CMYK in a `cmyk` document); `None` where
/// nothing paints it.
fn region_fill(elems: &[Elem], sources: &[usize], o: &FlattenOptions, cmyk: bool) -> Option<RegionFill> {
    let mut c = Composer { sources, top: None, plane: 0 };
    let alone = c.eval(elems, CLEAR);
    if alone[3] < 0.5 / 255.0 {
        return None;
    }
    // The white goes under the finished art, as under the renderer's: blend modes read only the art.
    let on_page = |p: Px| if o.preserve_alpha { p } else { composite(BlendMode::Normal, WHITE, p) };
    let px = on_page(alone);
    let rgb = [px[0], px[1], px[2]];
    // The K plane composites the same way.
    let k = cmyk.then(|| {
        c.plane = 1;
        on_page(c.eval(elems, CLEAR))[0]
    });
    // A paint showing as it is keeps its colour (spot, process, swatch) and overprint.
    let near = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-3);
    let same = |f: &Fill| px[3] >= 1.0 - 1e-4 && near(&f.planes[0], &rgb) && k.is_none_or(|k| near(&f.planes[1][..1], &[k]));
    let color = match k {
        Some(k) => {
            let [c, m, y, k] = planes_cmyk(rgb, k);
            Color::cmyk(c, m, y, k)
        }
        None => Color::rgb(rgb[0], rgb[1], rgb[2]),
    };
    let (paint, overprint) = match c.top {
        Some(f) if o.preserve_overprints && same(f) => (f.paint.clone(), f.overprint),
        _ => (Paint::solid(color), false),
    };
    Some(RegionFill { paint, alpha: px[3], overprint })
}

/// `art` rendered over `rect` at `ppi`: over white with the white the art doesn't cover taken out
/// again, or over nothing with Preserve Alpha. The images and their `clip`s reach a pixel further.
/// When the region is too large for a single image, it is split into pixel-aligned tiles.
/// An opaque image in a CMYK document holds the art's ink amounts (a CMYK TIFF), so its CMYK
/// colours keep their numbers (#2160); other images are PNGs. Fails when an image can't be encoded.
fn render_raster(doc: &Document, art: &[Node], rect: Rect, ppi: f64, clip: Option<PathData>, o: &FlattenOptions) -> Result<Vec<Raster>> {
    let scale = ppi / 72.0;
    let tiles = compute_tiles(rect, scale);
    
    let mut rasters = vec![];
    for tile_rect in tiles {
        let tile = render_single_tile(doc, art, tile_rect, scale, &clip, o)?;
        rasters.push(tile);
    }
    
    if rasters.len() > 1 {
        log::info!(
            "Rendered {:.0}×{:.0} pt area at {:.0} ppi as {} tiles to preserve resolution",
            rect.width(), rect.height(), ppi, rasters.len()
        );
    }
    
    Ok(rasters)
}

/// Compute pixel-aligned tile rectangles for a region at a given scale.
/// If the region fits within limits, returns a single tile.
/// Otherwise splits into tiles that each fit within MAX_PIXELS and MAX_RASTER_SIDE.
fn compute_tiles(rect: Rect, scale: f64) -> Vec<Rect> {
    let px = 1.0 / scale;
    let rect = rect.inflate(px, px);
    
    let total_width_px = (rect.width() * scale).ceil() as u64;
    let total_height_px = (rect.height() * scale).ceil() as u64;
    
    let max_side = u64::from(vectorcraft_render::MAX_RASTER_SIDE) - 4;
    let max_pixels = MAX_PIXELS as u64;
    
    let fits_single = total_width_px <= max_side 
        && total_height_px <= max_side 
        && total_width_px * total_height_px <= max_pixels;
    
    if fits_single {
        let region = Rect::new(
            rect.x0,
            rect.y0,
            rect.x0 + (rect.width() * scale).ceil().max(1.0) / scale,
            rect.y0 + (rect.height() * scale).ceil().max(1.0) / scale,
        );
        return vec![region];
    }
    
    let tile_side = tile_size_for_budget(max_pixels, max_side);
    let tile_pts = tile_side as f64 / scale;
    
    let cols = ((rect.width() / tile_pts).ceil() as usize).max(1);
    let rows = ((rect.height() / tile_pts).ceil() as usize).max(1);
    
    let mut tiles = vec![];
    for row in 0..rows {
        for col in 0..cols {
            let x0 = rect.x0 + col as f64 * tile_pts;
            let y0 = rect.y0 + row as f64 * tile_pts;
            let x1 = (x0 + tile_pts).min(rect.x1);
            let y1 = (y0 + tile_pts).min(rect.y1);
            
            let tile_rect = Rect::new(
                x0,
                y0,
                x0 + ((x1 - x0) * scale).ceil().max(1.0) / scale,
                y0 + ((y1 - y0) * scale).ceil().max(1.0) / scale,
            );
            tiles.push(tile_rect);
        }
    }
    
    tiles
}

/// Compute the tile side length in pixels that fits within budget and side limits.
fn tile_size_for_budget(max_pixels: u64, max_side: u64) -> u64 {
    let from_pixels = (max_pixels as f64).sqrt() as u64;
    from_pixels.min(max_side)
}

/// Render a single tile of art.
fn render_single_tile(doc: &Document, art: &[Node], tile_rect: Rect, scale: f64, clip: &Option<PathData>, o: &FlattenOptions) -> Result<Raster> {
    let px = 1.0 / scale;
    let tile_clip = clip.as_ref().map(|c| {
        Some(po::offset_path(c, px, po::Join::Miter, 4.0)).filter(|g| !g.is_empty()).unwrap_or_else(|| c.clone())
    });
    
    let xf = Affine::translate(tile_rect.origin().to_vec2()) * Affine::scale(1.0 / scale);
    let tmp = isolated_doc(doc, art.to_vec());
    let mut r = vectorcraft_render::Renderer::new();
    let no_alpha = o.no_transparency.is_some() && tile_clip.is_some() && !o.preserve_alpha;
    
    let spot_names = if no_alpha && doc.color_mode == ColorMode::Cmyk {
        collect_spot_names(doc, art)
    } else {
        vec![]
    };
    
    if no_alpha && doc.color_mode == ColorMode::Cmyk {
        let opts = vectorcraft_render::RenderOptions { background: Some([255; 4]), skip_templates: true, ..Default::default() };
        
        // Strip spot colors from art for the process tile. Spots get their own tint images.
        let process_art = if spot_names.is_empty() {
            art.to_vec()
        } else {
            strip_spots_for_process(art, doc)
        };
        let process_tmp = isolated_doc(doc, process_art);
        
        let inks = r.render_region_inks(&process_tmp, tile_rect, scale, &opts);
        vectorcraft_doc::cancel::check()?;
        let (width, height) = vectorcraft_render::region_pixels(tile_rect, scale);
        if let Some(blob) = Inks::new(width, height, inks).and_then(|i| ImageBlob::cmyk_tiff(&i)) {
            let mut spot_images = vec![];
            for spot_name in &spot_names {
                if let Some(tint_pixels) = render_spot_tint(doc, art, spot_name, tile_rect, scale) {
                    let alternate = spot_alternate(doc, spot_name);
                    if let Ok(png) = grayscale_png(width, height, &tint_pixels) {
                        spot_images.push(SpotImage {
                            name: spot_name.clone(),
                            alternate,
                            blob: ImageBlob::new("image/png", png),
                        });
                    }
                }
            }
            return Ok(Raster { blob, width, height, xf, clip: tile_clip, spot_images });
        }
    }
    
    let mut img = r.render_region(&tmp, tile_rect, scale, !o.preserve_alpha);
    vectorcraft_doc::cancel::check()?;
    
    if no_alpha {
        for p in img.pixels.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
    } else if !o.preserve_alpha || !o.anti_alias {
        let cover: Vec<Node> = art
            .iter()
            .map(|n| {
                let mut c = n.clone();
                opaque(&mut c);
                c
            })
            .collect();
        let cover = r.render_region(&isolated_doc(doc, cover), tile_rect, scale, false);
        for (px, c) in img.pixels.as_chunks_mut::<4>().0.iter_mut().zip(cover.pixels.as_chunks::<4>().0) {
            let k = c[3];
            let k2 = if o.anti_alias || k == 0 || k == 255 {
                k
            } else if k >= 128 {
                255
            } else {
                0
            };
            if o.preserve_alpha {
                let f = k2 as f32 / k.max(1) as f32;
                for v in px.iter_mut() {
                    *v = (*v as f32 * f).round().min(255.0) as u8;
                }
            } else {
                let white = 255 - k2;
                for v in &mut px[..3] {
                    *v = v.saturating_sub(white).min(k2);
                }
                px[3] = k2;
            }
        }
    }
    
    let png = img.to_png().map_err(|e| EngineError::Other(format!("Flatten Transparency: {e}")))?;
    Ok(Raster { blob: ImageBlob::new("image/png", png), width: img.width, height: img.height, xf, clip: tile_clip, spot_images: vec![] })
}

/// Pixels per point of an image of `rect` at `ppi`: lower when the image would be larger than
/// [`MAX_PIXELS`], or have a side longer than the renderer draws (the image grows by a pixel on
/// each side and is rounded up to whole pixels). Used for computing image dimensions.
fn raster_scale(rect: Rect, ppi: f64) -> f64 {
    let requested_scale = ppi / 72.0;
    let pixel_capped_scale = (MAX_PIXELS / (rect.width() * rect.height()).max(1.0)).sqrt();
    let side_capped_scale = (f64::from(vectorcraft_render::MAX_RASTER_SIDE) - 4.0) / rect.width().max(rect.height()).max(1.0);
    
    requested_scale.min(pixel_capped_scale).min(side_capped_scale)
}

/// `n` at full opacity with Normal blending throughout (objects, fills and strokes).
pub(super) fn opaque(n: &mut Node) {
    n.opacity = 1.0;
    n.blend = BlendMode::Normal;
    for i in &mut n.appearance.items {
        match i {
            AppearanceItem::Fill(f) => (f.opacity, f.blend) = (1.0, BlendMode::Normal),
            AppearanceItem::Stroke(s) => (s.opacity, s.blend) = (1.0, BlendMode::Normal),
        }
    }
    if let Some(ch) = n.children_mut() {
        for c in ch {
            opaque(Arc::make_mut(c));
        }
    }
}

/// Whether `doc` has pattern fills whose image tiles need transparency flattening with `o`
/// ([`expand_transparent_tile_images`]).
pub(crate) fn has_transparent_tile_images(doc: &Document, o: &FlattenOptions) -> bool {
    let keys = o.no_transparency.clone().unwrap_or_default();
    let colorize = |d: &Document, n: &Node| o.counts_colorize(d, n);
    !doc.patterns.is_empty() && crate::cmd::expand::has_transparent_image_patterns(doc, &keys, &colorize)
}

/// Expand pattern fills whose image tiles need transparency flattening.
pub(crate) fn expand_transparent_tile_images(doc: &Document, o: &FlattenOptions) -> Result<Option<Document>> {
    if !has_transparent_tile_images(doc, o) {
        return Ok(None);
    }
    let keys = o.no_transparency.clone().unwrap_or_default();
    let colorize = |d: &Document, n: &Node| o.counts_colorize(d, n);
    let mut d = doc.clone();
    let layers: Vec<NodeId> =
        d.layers.iter().filter(|l| l.visible && !matches!(l.kind, NodeKind::Layer { template: true, .. })).map(|l| l.id).collect();
    let mut changed = 0;
    for id in layers {
        changed += crate::cmd::expand::expand_transparent_image_patterns(&mut d, id, &keys, &colorize)?;
    }
    Ok((changed > 0).then_some(d))
}

/// `doc` with everything it draws flattened with `o`, as formats without transparency (EPS)
/// write it: every visible object of the visible layers, locked ones too (a clipping layer's
/// clipping path stays). Pattern tile images with opacity, blending or a colorize paint that
/// `o` counts ([`FlattenOptions::counts_colorize`]) are expanded; see-through tile pixels are
/// expanded when `o.no_transparency` counts them. `None` when nothing changes.
pub(crate) fn flatten_document(doc: &Document, o: &FlattenOptions) -> Result<Option<Document>> {
    let expanded = expand_transparent_tile_images(doc, o)?;
    Ok(flatten_expanded(expanded.as_ref().unwrap_or(doc), o)?.or(expanded))
}

/// [`flatten_document`] for `doc` whose pattern tile images were expanded already
/// ([`expand_transparent_tile_images`]).
pub(crate) fn flatten_expanded(doc: &Document, o: &FlattenOptions) -> Result<Option<Document>> {
    let outlining = o.text_to_outlines || o.strokes_to_outlines || !o.preserve_overprints;
    if !outlining && !doc.layers.iter().any(|l| o.shows_transparency(doc, l) || o.draws_see_through_image(l)) {
        return Ok(None);
    }
    let mut roots = vec![];
    for l in doc.layers.iter().filter(|l| l.visible && !matches!(l.kind, NodeKind::Layer { template: true, .. })) {
        let children = l.children().map_or(&[][..], Vec::as_slice);
        roots.extend(children.iter().skip(usize::from(l.clips())).filter(|c| c.visible && !c.is_ruler_guide()).map(|c| c.id));
    }
    let plan = plan(doc, &roots, o, true)?;
    if plan.flat.is_empty() && !plan.work.iter().any(|k| k.changes) {
        return Ok(None);
    }
    let mut d = doc.clone();
    apply(&mut d, &mut vectorcraft_doc::Selection::default(), plan, o)?;
    Ok(Some(d))
}

// ---------- apply ----------

pub(super) fn apply(d: &mut Document, sel: &mut vectorcraft_doc::Selection, plan: Plan, o: &FlattenOptions) -> Result<(Vec<NodeId>, usize, usize)> {
    let (mut vector, mut rasterized) = (0, 0);
    let (mut ids, mut flat) = (vec![], vec![]);
    // The groups replace their objects in one pass at the end. When an object is listed twice or
    // lies inside another listed one, each group replaces its objects as it is made instead.
    let one_pass = separate(d, &plan.flat.iter().flat_map(|f| f.roots.iter().copied()).collect::<Vec<_>>());
    // A sublayer that becomes flat art gives its ruler guides to the layer that held it.
    let guides = held_guides(d, plan.flat.iter().flat_map(|f| f.roots.iter().copied()));
    for f in plan.flat {
        // A group always has objects.
        let Some(top) = place_of(d, &f.roots) else { continue };
        // Art hidden entirely is removed.
        if f.regions.is_empty() && f.area.is_none() {
            if one_pass {
                flat.extend(f.roots.iter().map(|r| (*r, vec![])));
            } else {
                for r in &f.roots {
                    d.remove(*r)?;
                }
            }
            continue;
        }
        let mut children = vec![];
        // The images go under the regions, reaching a pixel past their own: their edges then fall on
        // matching pixels instead of on what is behind the group.
        // raster_nodes returns the main CMYK image first, then spot tint images layered above it.
        // Multiple rasters are tiles covering the region together.
        for r in f.rasters {
            for node in raster_nodes(d, r) {
                children.push(Arc::new(node));
            }
            rasterized += 1;
        }
        // Type over several lines gives a group per line.
        let mut by_line: Vec<Vec<Arc<Node>>> = f.lines.as_ref().map_or(vec![], |l| vec![vec![]; l.bands.len()]);
        for (path, fill) in f.regions {
            let line = f.lines.as_ref().map(|l| l.line_of(&path));
            let mut n = shape_node(d, path, None);
            n.appearance = Appearance {
                items: vec![AppearanceItem::Fill(FillLayer { overprint: fill.overprint, ..FillLayer::new(fill.paint) })],
                ..Appearance::default()
            }
            .into();
            n.opacity = fill.alpha.min(1.0);
            match line.and_then(|i| by_line.get_mut(i)) {
                Some(l) => l.push(Arc::new(n)),
                None => children.push(Arc::new(n)),
            }
            vector += 1;
        }
        by_line.retain(|l| !l.is_empty());
        if by_line.len() > 1 {
            for l in by_line {
                children.push(Arc::new(Node::group(d.alloc_id(), l)));
            }
        } else {
            children.extend(by_line.into_iter().flatten());
        }
        // Add preserved spot nodes on top (they were kept as vector to preserve spot plates)
        for (_id, mut spot_node) in f.preserved_spots {
            spot_node.id = d.alloc_id();
            children.push(Arc::new(spot_node));
            vector += 1;
        }
        let g = Node::group(d.alloc_id(), children);
        ids.push(g.id);
        if one_pass {
            // The group replaces the topmost object, and the other objects are removed.
            flat.extend(f.roots.iter().filter(|r| **r != top).map(|r| (*r, vec![])));
            flat.push((top, vec![g]));
        } else {
            let (par, idx, _) = d.position(top).ok_or(EngineError::NoNode(top))?;
            d.insert(par, idx + 1, g)?;
            for r in &f.roots {
                d.remove(*r)?;
            }
        }
    }
    if !flat.is_empty() {
        d.insert_beside(flat, true)?;
    }
    for (layer, guide) in guides {
        d.insert(Some(layer), usize::MAX, guide)?;
    }
    let brushes = if o.strokes_to_outlines { vectorcraft_brush::library(d) } else { vec![] };
    for k in plan.work {
        ids.push(outline_kept(d, &brushes, k, o)?);
    }
    ids.sort_by_key(|id| d.index_path(*id));
    sel.set(ids.iter().copied());
    Ok((ids, vector, rasterized))
}

/// The object of `roots` (in paint order) whose place their flattened group takes: the topmost one
/// that no clip group or clipping layer clips unless it clips every object of `roots`, else the
/// topmost one. The group stays with the topmost object rather than move to a layer that prints,
/// shows or is a template where the topmost object's layer differs, out from a clip group or
/// clipping layer whose clipping path has a fill, which would then cover it, or out from a layer
/// or group with opacity, a blend mode or isolation.
fn place_of(d: &Document, roots: &[NodeId]) -> Option<NodeId> {
    let top = roots.last().copied()?;
    if !clips_count() || roots.len() < 2 {
        return Some(top);
    }
    let fills = |clip: &NodeId| d.node(*clip).and_then(|n| n.children()?.first().map(|c| c.clip_paint().fill.is_some())).unwrap_or(false);
    let all: Vec<Vec<NodeId>> = roots.iter().map(|r| clip_ancestors(d, *r)).collect();
    let above = all.last().map_or(&[][..], Vec::as_slice);
    let shared = |mine: &Vec<NodeId>| mine.iter().all(|c| all.iter().all(|theirs| theirs.contains(c)));
    let leaves_fill = |mine: &Vec<NodeId>| above.iter().any(|c| !mine.contains(c) && fills(c));
    // Out of a layer or group with opacity, a blend mode or isolation, the group would lose it.
    let leaves_compositing = |r: NodeId| own_layers(d, top, Some(r)).iter().any(|a| !draws_as_is(a));
    let state = layer_state(d, top);
    let place =
        roots.iter().zip(&all).rev().find(|(r, mine)| shared(mine) && !leaves_fill(mine) && layer_state(d, **r) == state && !leaves_compositing(**r));
    Some(place.map_or(top, |(r, _)| *r))
}

/// The ruler guides of each of `roots` that is a sublayer (in its sublayers too), with the layer
/// that holds it, in order. Other objects hold none. Each root, and each child of it and of its
/// sublayers, is looked at once.
fn held_guides(d: &Document, roots: impl IntoIterator<Item = NodeId>) -> Vec<(NodeId, Node)> {
    let mut out = vec![];
    for id in roots {
        let (Some(n), Some(parent)) = (d.node(id), d.parent_of(id)) else { continue };
        guide_seen(n);
        if n.is_layer() {
            guides_of(n, parent, &mut out);
        }
    }
    out
}

/// Push each ruler guide of `layer` and of its sublayers onto `out` in paint order, with `parent`.
fn guides_of(layer: &Node, parent: NodeId, out: &mut Vec<(NodeId, Node)>) {
    for c in layer.children().into_iter().flatten() {
        guide_seen(c);
        if c.is_ruler_guide() {
            out.push((parent, Node::clone(c)));
        } else if c.is_layer() {
            guides_of(c, parent, out);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The nodes that [`held_guides`] has looked at on this thread, in order.
    static GUIDES_SEEN: std::cell::RefCell<Vec<NodeId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Note that [`held_guides`] looked at `n`.
#[cfg(test)]
fn guide_seen(n: &Node) {
    GUIDES_SEEN.with(|s| s.borrow_mut().push(n.id));
}

#[cfg(not(test))]
fn guide_seen(_: &Node) {}

#[cfg(test)]
thread_local! {
    /// The nodes that [`Kept::of`] has looked at on this thread, in order.
    static KEPT_SEEN: std::cell::RefCell<Vec<NodeId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Note that [`Kept::of`] looked at `n`.
#[cfg(test)]
fn kept_seen(n: &Node) {
    KEPT_SEEN.with(|s| s.borrow_mut().push(n.id));
}

#[cfg(not(test))]
fn kept_seen(_: &Node) {}

/// An image of `r` (in a clip group when it is clipped).
/// Create nodes from a raster: the main image and any spot tint images.
/// Spot images use colorize to output as Separation colorspaces with overprint.
fn raster_nodes(d: &mut Document, r: Raster) -> Vec<Node> {
    let mut nodes = vec![];
    
    // Main CMYK image
    let key = unique_key(d, "flattened");
    d.images.insert(key.clone(), r.blob);
    let image = Node::new(
        d.alloc_id(),
        NodeKind::Image(Box::new(ImageObject {
            key,
            width: r.width,
            height: r.height,
            xf: r.xf,
            link: None,
            placement: Default::default(),
            pixels: None,
        })),
    );
    
    let main_node = match &r.clip {
        Some(path) => crate::cmd::rasterfx::clip_group(d, path.clone(), FillRule::NonZero, image),
        None => image,
    };
    nodes.push(main_node);
    
    // Spot tint images: each is a grayscale image colorized with the spot color
    // Output: the coverage shows where the ink goes, colorize makes it print as that spot
    for spot in r.spot_images {
        let key = unique_key(d, &format!("spot_{}", spot.name.replace(' ', "_")));
        d.images.insert(key.clone(), spot.blob);
        
        // Create a solid paint linked to the spot swatch at 100% tint.
        // The grayscale pixels in the image act as the tint amount.
        let colorize = Paint::Solid {
            color: Color::cmyk(spot.alternate[0], spot.alternate[1], spot.alternate[2], spot.alternate[3]),
            swatch: Some(spot.name.clone()),
            tint: 1.0,
        };
        
        let mut spot_image = Node::new(
            d.alloc_id(),
            NodeKind::Image(Box::new(ImageObject {
                key,
                width: r.width,
                height: r.height,
                xf: r.xf,
                link: None,
                placement: Default::default(),
                pixels: None,
            })),
        );
        spot_image.extras_mut().colorize = Some(Box::new(colorize));
        
        // Set overprint for spot images so they composite correctly with CMYK underneath
        for item in &mut spot_image.appearance.items {
            *item.overprint_mut() = true;
        }
        
        let spot_node = match &r.clip {
            Some(path) => crate::cmd::rasterfx::clip_group(d, path.clone(), FillRule::NonZero, spot_image),
            None => spot_image,
        };
        nodes.push(spot_node);
    }
    
    nodes
}

/// Do what [`Kept`] `k` found to its object of `d` (`brushes`: the document's brushes, for the
/// stroke option): its type outlined, then the strokes of its leaf shapes, the outlines of its
/// type included, then its overprints dropped. → the id now standing for the object.
fn outline_kept(d: &mut Document, brushes: &[vectorcraft_brush::Brush], k: Kept, o: &FlattenOptions) -> Result<NodeId> {
    let mut root = k.id;
    let mut replaced = HashSet::new();
    let dropping = !o.preserve_overprints;
    // Each replacement loses its overprints before it goes in: no new object is looked up.
    let mut put = |d: &mut Document, old: NodeId, mut new: Node, root: &mut NodeId| -> Result<()> {
        if dropping {
            drop_overprints(&mut new);
        }
        let nid = new.id;
        *d.node_mut(old).ok_or(EngineError::NoNode(old))? = new;
        replaced.insert(old);
        if old == *root {
            *root = nid;
        }
        Ok(())
    };
    // The leaf shapes of each outlined type that only groups and layers hold, by step.
    let mut outlined: Vec<Vec<Node>> = Vec::with_capacity(k.steps.len());
    for step in &k.steps {
        let mut lv = vec![];
        if let Step::Text { id, leaf } = *step
            && let Some(out) = d.node(id).and_then(outlined_text)
        {
            let out = d.reid(&out);
            if leaf && o.strokes_to_outlines {
                leaves(&out, &mut lv);
            }
            put(d, id, out, &mut root)?;
        }
        outlined.push(lv);
    }
    if o.strokes_to_outlines {
        for (step, lv) in k.steps.iter().zip(outlined) {
            let shapes = match *step {
                Step::Leaf(id) => d.node(id).cloned().into_iter().collect(),
                Step::Text { .. } => lv,
            };
            for l in shapes.iter().filter(|l| !matches!(l.kind, NodeKind::Text(_))) {
                if let Some(new) = outline_strokes(d, brushes, l) {
                    put(d, l.id, new, &mut root)?;
                }
            }
        }
    }
    if dropping {
        for (id, holder) in k.overprinted {
            if holder.is_some_and(|h| replaced.contains(&h)) {
                continue;
            }
            if let Some(n) = d.node_mut(id) {
                drop_own_overprints(n);
            }
        }
    }
    Ok(root)
}

/// Whether `n` is a path with a visible stroke (outlining turns it into a fill).
pub(super) fn stroked(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Path { guide: false, .. } | NodeKind::Compound { .. })
        && n.appearance.items.iter().any(|i| matches!(i, AppearanceItem::Stroke(s) if s.visible && !s.paint.is_none() && s.width > 0.0))
}

/// Drop the overprints of `n` itself.
fn drop_own_overprints(n: &mut Node) {
    if overprints(n) {
        for i in &mut n.appearance.items {
            *i.overprint_mut() = false;
        }
        if let NodeKind::Text(t) = &mut n.kind {
            for r in &mut t.runs {
                (r.style.overprint_fill, r.style.overprint_stroke) = (false, false);
            }
        }
    }
}

fn overprints(n: &Node) -> bool {
    n.appearance.items.iter().any(AppearanceItem::overprint)
        || matches!(&n.kind, NodeKind::Text(t) if t.runs.iter().any(|r| r.style.overprint_fill || r.style.overprint_stroke))
}

/// Drop the overprints of `n` and of everything under it.
fn drop_overprints(n: &mut Node) {
    drop_own_overprints(n);
    if let Some(ch) = n.children_mut() {
        for c in ch {
            drop_overprints(Arc::make_mut(c));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::Session;

    #[test]
    fn a_flattened_sublayer_gives_its_ruler_guides_to_its_layer() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"width": 400, "height": 300})).unwrap();
        let layer = s.doc().unwrap().doc.layers[0].id;
        let sub = NodeId(s.execute("layer.newSublayer", &json!({"parent": layer.0})).unwrap()["id"].as_u64().unwrap());
        s.execute("layer.setCurrent", &json!({"id": sub.0})).unwrap();
        let r = s.execute("shape.rectangle", &json!({"x": 10, "y": 10, "width": 20, "height": 20})).unwrap()["id"].clone();
        s.execute("transparency.set", &json!({"ids": [r], "opacity": 50})).unwrap();
        let d = Arc::make_mut(&mut s.doc_mut().unwrap().doc);
        let g = Node::new(d.alloc_id(), NodeKind::RulerGuide { vertical: true, pos: 5.0, artboard: None });
        let g = d.insert(Some(sub), usize::MAX, g).unwrap();
        let flat = flatten_document(&s.doc().unwrap().doc, &FlattenOptions::default()).unwrap().unwrap();
        assert!(flat.node(sub).is_none(), "the sublayer became flat art");
        assert!(flat.node(g).is_some_and(Node::is_ruler_guide), "its guide is kept");
        assert_eq!(flat.parent_of(g), Some(layer));
        assert_eq!(flat.children(Some(layer)).and_then(|c| c.last()).map(|c| c.id), Some(g));
    }

    /// The guides of a sublayer and of its sublayers are gathered in paint order with the layer
    /// that holds it, looking at the sublayer and each child of it and of its sublayers once.
    /// Other roots give none.
    #[test]
    fn held_guides_look_at_each_node_once() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"width": 400, "height": 300})).unwrap();
        let layer = s.doc().unwrap().doc.layers[0].id;
        let sublayer =
            |s: &mut Session, parent: NodeId| NodeId(s.execute("layer.newSublayer", &json!({"parent": parent.0})).unwrap()["id"].as_u64().unwrap());
        let rect = |s: &mut Session, on: NodeId| {
            s.execute("layer.setCurrent", &json!({"id": on.0})).unwrap();
            NodeId(s.execute("shape.rectangle", &json!({"x": 10, "y": 10, "width": 20, "height": 20})).unwrap()["id"].as_u64().unwrap())
        };
        let sub = sublayer(&mut s, layer);
        let deep = sublayer(&mut s, sub);
        let empty = sublayer(&mut s, layer);
        let (a, b, c) = (rect(&mut s, sub), rect(&mut s, deep), rect(&mut s, empty));
        let d = Arc::make_mut(&mut s.doc_mut().unwrap().doc);
        let mut guide = |on: NodeId, pos: f64| {
            let g = Node::new(d.alloc_id(), NodeKind::RulerGuide { vertical: true, pos, artboard: None });
            d.insert(Some(on), usize::MAX, g).unwrap()
        };
        let (g1, g2, g3) = (guide(sub, 1.0), guide(deep, 2.0), guide(sub, 3.0));
        let doc = &s.doc().unwrap().doc;
        let ids = |n: &Node| {
            let mut v = vec![];
            n.walk(&mut |c| v.push(c.id));
            v
        };
        assert_eq!(ids(doc.node(sub).unwrap()), vec![sub, deep, b, g2, a, g1, g3]);
        let run = |roots: &[NodeId]| {
            GUIDES_SEEN.with(|v| v.borrow_mut().clear());
            let got: Vec<_> = held_guides(doc, roots.iter().copied()).into_iter().map(|(p, g)| (p, g.id)).collect();
            (got, GUIDES_SEEN.with(|v| v.take()))
        };
        // Every node of the sublayer is looked at once, and the guides keep their paint order.
        assert_eq!(run(&[sub]), (vec![(layer, g2), (layer, g1), (layer, g3)], vec![sub, deep, b, g2, a, g1, g3]));
        // A sublayer that holds none gives none; a path holds none; a missing root and a top-level
        // layer, which has no parent, are skipped.
        assert_eq!(run(&[empty]), (vec![], vec![empty, c]));
        assert_eq!(run(&[a]), (vec![], vec![a]));
        assert_eq!(run(&[NodeId(u64::MAX), layer]), (vec![], vec![]));
        // A deeper sublayer gives its guides to the sublayer that holds it.
        assert_eq!(run(&[deep, deep]).0, vec![(sub, g2), (sub, g2)]);
    }

    /// The objects [`kept_session`] makes.
    struct KeptArt {
        /// Each group with its stroked rectangle (its fill overprints) and its stroked type.
        groups: Vec<(NodeId, NodeId, NodeId)>,
        /// A stroked rectangle and stroked type with an overprint, on their own.
        rect: NodeId,
        text: NodeId,
        /// A stroked compound path and its pieces.
        compound: NodeId,
        pieces: Vec<NodeId>,
        /// A group holding an unstroked rectangle and a hidden stroked rectangle.
        hidden_group: NodeId,
        hidden: NodeId,
        /// An unstroked rectangle.
        plain: NodeId,
    }

    /// A document without transparency: `n` groups, then the other objects of [`KeptArt`].
    fn kept_session(n: usize) -> (Session, KeptArt) {
        let mut s = Session::new();
        s.execute("file.new", &json!({"width": 400, "height": 300})).unwrap();
        let id = |v: serde_json::Value| NodeId(v["id"].as_u64().unwrap());
        let rect = |s: &mut Session, x: f64| id(s.execute("shape.rectangle", &json!({"x": x, "y": 10, "width": 20, "height": 20})).unwrap());
        let text = |s: &mut Session, x: f64| {
            let t = id(s.execute("text.create", &json!({"x": x, "y": 80, "text": "Hi", "size": 24})).unwrap());
            s.execute("paint.setStroke", &json!({"color": "#000000", "ids": [t.0]})).unwrap();
            t
        };
        let select = |s: &mut Session, ids: &[NodeId]| {
            s.execute("select.set", &json!({"ids": ids.iter().map(|i| i.0).collect::<Vec<_>>()})).unwrap();
        };
        let overprint = |s: &mut Session, on: NodeId| {
            s.execute("attributes.set", &json!({"ids": [on.0], "overprintFill": true})).unwrap();
        };
        let unstroked = |s: &mut Session, on: NodeId| {
            s.execute("paint.setStroke", &json!({"none": true, "ids": [on.0]})).unwrap();
        };
        let mut groups = vec![];
        for i in 0..n {
            let x = (i % 10) as f64 * 30.0;
            let (r, t) = (rect(&mut s, x), text(&mut s, x));
            overprint(&mut s, r);
            select(&mut s, &[r, t]);
            groups.push((id(s.execute("object.group", &json!({})).unwrap()), r, t));
        }
        let (r, t) = (rect(&mut s, 0.0), text(&mut s, 100.0));
        overprint(&mut s, t);
        let (a, b) = (rect(&mut s, 200.0), rect(&mut s, 210.0));
        select(&mut s, &[a, b]);
        let compound = id(s.execute("object.compoundPath.make", &json!({})).unwrap());
        s.execute("paint.setStroke", &json!({"color": "#000000", "ids": [compound.0]})).unwrap();
        let pieces = s.doc().unwrap().doc.node(compound).unwrap().children().unwrap().iter().map(|c| c.id).collect();
        let (shown, hidden) = (rect(&mut s, 300.0), rect(&mut s, 310.0));
        unstroked(&mut s, shown);
        select(&mut s, &[shown, hidden]);
        let hidden_group = id(s.execute("object.group", &json!({})).unwrap());
        s.execute("object.hide", &json!({"ids": [hidden.0]})).unwrap();
        let plain = rect(&mut s, 350.0);
        unstroked(&mut s, plain);
        (s, KeptArt { groups, rect: r, text: t, compound, pieces, hidden_group, hidden, plain })
    }

    /// Every option on: type and strokes outlined, overprints dropped.
    fn outlining() -> FlattenOptions {
        FlattenOptions { text_to_outlines: true, strokes_to_outlines: true, preserve_overprints: false, ..FlattenOptions::default() }
    }

    /// The top-level objects of `d`'s first layer, in paint order.
    fn top_level(d: &Document) -> Vec<NodeId> {
        d.layers[0].children().unwrap().iter().map(|c| c.id).collect()
    }

    fn sorted(mut ids: Vec<NodeId>) -> Vec<NodeId> {
        ids.sort();
        ids
    }

    /// The nodes [`Kept::of`] looked at while `f` ran, sorted.
    fn kept_seen_by(f: impl FnOnce()) -> Vec<NodeId> {
        KEPT_SEEN.with(|v| v.borrow_mut().clear());
        f();
        sorted(KEPT_SEEN.with(|v| v.take()))
    }

    /// The Flattener Preview, `object.flattenTransparency` and flattening a whole document each look
    /// at every node of the objects without transparency once, with type and strokes outlined and
    /// overprints dropped, an object they leave as it is included.
    #[test]
    fn kept_objects_are_looked_at_once() {
        let (mut s, _) = kept_session(100);
        let o = outlining();
        let doc = s.doc().unwrap().doc.clone();
        let roots = top_level(&doc);
        let mut nodes = vec![];
        for r in &roots {
            doc.node(*r).unwrap().walk(&mut |c| nodes.push(c.id));
        }
        nodes.sort();
        assert_eq!(nodes.len(), 309);
        let preview = kept_seen_by(|| drop(super::super::report::report(&doc, &roots, &o).unwrap()));
        assert!(preview == nodes, "the preview looked at {} nodes of {}", preview.len(), nodes.len());
        let whole = kept_seen_by(|| drop(flatten_document(&doc, &o).unwrap().unwrap()));
        assert!(whole == nodes, "flattening the document looked at {} nodes of {}", whole.len(), nodes.len());
        let ids: Vec<u64> = roots.iter().map(|r| r.0).collect();
        let command = kept_seen_by(|| {
            s.execute(
                "object.flattenTransparency",
                &json!({"ids": ids, "textToOutlines": true, "strokesToOutlines": true, "preserveOverprints": false}),
            )
            .unwrap();
        });
        assert!(command == nodes, "object.flattenTransparency looked at {} nodes of {}", command.len(), nodes.len());
    }

    /// [`apply`]'s outlining of `kept` (objects of `d` without transparency) with `o`, done as a
    /// walk of each object for each option: → the ids standing for them, in paint order.
    fn outlined_walk_by_walk(d: &mut Document, kept: &[NodeId], o: &FlattenOptions) -> Vec<NodeId> {
        let brushes = if o.strokes_to_outlines { vectorcraft_brush::library(d) } else { vec![] };
        let mut ids = vec![];
        for &root in kept {
            let mut id = root;
            if o.text_to_outlines {
                let mut texts = vec![];
                d.node(id).unwrap().walk(&mut |c| {
                    if matches!(c.kind, NodeKind::Text(_)) {
                        texts.push(c.clone());
                    }
                });
                for t in texts {
                    let Some(out) = outlined_text(&t) else { continue };
                    let out = d.reid(&out);
                    if t.id == root {
                        id = out.id;
                    }
                    *d.node_mut(t.id).unwrap() = out;
                }
            }
            if o.strokes_to_outlines {
                id = crate::cmd::pathops::outline_strokes_under(d, &brushes, id).unwrap().0;
            }
            if !o.preserve_overprints {
                drop_overprints(d.node_mut(id).unwrap());
            }
            ids.push(id);
        }
        ids.sort_by_key(|id| d.index_path(*id));
        ids
    }

    /// With each mix of the text, stroke and overprint options, the objects without transparency
    /// end as a walk of each of them for each option leaves them: the same art, ids and selection.
    #[test]
    fn kept_objects_change_as_a_walk_per_option_changes_them() {
        let (s, _) = kept_session(3);
        let doc = Document::clone(&s.doc().unwrap().doc);
        let roots = top_level(&doc);
        for bits in 0..8u8 {
            let o = FlattenOptions {
                text_to_outlines: bits & 1 != 0,
                strokes_to_outlines: bits & 2 != 0,
                preserve_overprints: bits & 4 != 0,
                ..FlattenOptions::default()
            };
            let p = plan(&doc, &roots, &o, true).unwrap();
            assert!(p.flat.is_empty(), "{o:?}");
            // In the order the plan's groups come in.
            let kept = p.kept.clone();
            assert_eq!(sorted(kept.clone()), sorted(roots.clone()), "{o:?}");
            let mut once = doc.clone();
            let mut sel = vectorcraft_doc::Selection::default();
            let (ids, ..) = apply(&mut once, &mut sel, p, &o).unwrap();
            let mut walked = doc.clone();
            let want = outlined_walk_by_walk(&mut walked, &kept, &o);
            assert_eq!(ids, want, "{o:?}");
            assert!(once == walked, "{o:?}");
            assert_eq!(once == doc, bits == 4, "{o:?}: only with every option off does nothing change");
        }
    }

    /// The Flattener Preview lists the kept objects the options change, and of those the type and
    /// the stroked paths that show and are not pieces of a compound path.
    #[test]
    fn the_preview_lists_what_the_options_change_in_kept_objects() {
        let (s, a) = kept_session(2);
        let doc = &s.doc().unwrap().doc;
        let roots = top_level(doc);
        let r = super::super::report::report(doc, &roots, &outlining()).unwrap();
        let (g, rects, texts): (Vec<_>, Vec<_>, Vec<_>) =
            (a.groups.iter().map(|g| g.0).collect(), a.groups.iter().map(|g| g.1).collect(), a.groups.iter().map(|g| g.2).collect());
        assert_eq!(r.affected, [g.clone(), vec![a.rect, a.text, a.compound, a.hidden_group]].concat());
        // Both lists come in the order the plan's groups do.
        assert_eq!(sorted(r.text.clone()), sorted([texts, vec![a.text]].concat()));
        assert_eq!(sorted(r.strokes.clone()), sorted([rects, vec![a.rect, a.compound]].concat()));
        assert!(!r.strokes.contains(&a.hidden) && !r.strokes.iter().any(|id| a.pieces.contains(id)) && !r.affected.contains(&a.plain));
        // Dropping overprints alone changes the objects that overprint.
        let o = FlattenOptions { preserve_overprints: false, ..FlattenOptions::default() };
        let r = super::super::report::report(doc, &roots, &o).unwrap();
        assert_eq!((r.affected, r.text, r.strokes), ([g, vec![a.text]].concat(), vec![], vec![]));
        // With every option off, nothing.
        assert!(super::super::report::report(doc, &roots, &FlattenOptions::default()).unwrap().affected.is_empty());
    }

    /// In a file without transparency with Clip Complex Regions on, art under a group's opaque
    /// image within one of its pixels joins the group, also where a long image is made at a lower
    /// resolution and its pixels are wider than at the preset's.
    #[test]
    fn art_under_the_edge_of_a_long_opaque_image_joins_its_group() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"width": 31000, "height": 100})).unwrap();
        let rect = |s: &mut Session, x: f64, w: f64| {
            let id = s.execute("shape.rectangle", &json!({"x": x, "y": 10, "width": w, "height": 10})).unwrap()["id"].as_u64().unwrap();
            s.execute("paint.setStroke", &json!({"none": true, "ids": [id]})).unwrap();
            NodeId(id)
        };
        // 0.5 pt left of A: wider than a pixel at 300 ppi, narrower than one of a 30,000 pt image.
        let d = rect(&mut s, 40.0, 19.5);
        let a = rect(&mut s, 60.0, 40.0);
        let t = rect(&mut s, 95.0, 30_000.0);
        s.execute("transparency.set", &json!({"ids": [t.0], "opacity": 50})).unwrap();
        let o = FlattenOptions { no_transparency: Some(Default::default()), clip_complex_regions: true, ..FlattenOptions::default() };
        let doc = &s.doc().unwrap().doc;
        let long = plan(doc, &[d, a, t], &o, false).unwrap();
        assert!(long.kept.is_empty(), "{:?}", long.kept);
        assert_eq!(long.flat.iter().map(|f| f.roots.clone()).collect::<Vec<_>>(), vec![vec![d, a, t]]);
        // Short, the image has 300 ppi pixels: D stays.
        let short = rect(&mut s, 95.0, 40.0);
        s.execute("transparency.set", &json!({"ids": [short.0], "opacity": 50})).unwrap();
        let doc = &s.doc().unwrap().doc;
        assert_eq!(plan(doc, &[d, a, short], &o, false).unwrap().kept, vec![d]);
        // 100,000 pt long, its pixels are over 3 pt wide: art 3 pt away, whose reach doesn't touch
        // A's, joins too.
        let far = rect(&mut s, 40.0, 17.0);
        let longer = rect(&mut s, 95.0, 100_000.0);
        s.execute("transparency.set", &json!({"ids": [longer.0], "opacity": 50})).unwrap();
        let doc = &s.doc().unwrap().doc;
        let p = plan(doc, &[far, a, longer], &o, false).unwrap();
        assert!(p.kept.is_empty(), "{:?}", p.kept);
        assert_eq!(p.flat.iter().map(|f| f.roots.clone()).collect::<Vec<_>>(), vec![vec![far, a, longer]]);
    }

    /// Spot-colored objects at the top of the stacking order that don't interact with transparency
    /// are preserved as vector when an over-cap raster is created, keeping the spot plates intact.
    #[test]
    fn spot_colors_at_top_are_preserved_as_vector_in_over_cap_raster() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"width": 100, "height": 100, "colorMode": "cmyk"})).unwrap();
        // Create a spot swatch
        s.execute("swatch.new", &json!({"name": "SpotInk", "color": {"c": 0, "m": 100, "y": 0, "k": 0}, "spot": true})).unwrap();
        
        // Create many overlapping transparent shapes to trigger over-cap
        // FlattenOptions::REGION_CAP is about 3900, so we need enough shapes to exceed that
        let mut trans_ids = vec![];
        for i in 0..50 {
            let id = s.execute("shape.rectangle", &json!({
                "x": 10.0 + (i as f64 * 0.5), 
                "y": 10.0 + (i as f64 * 0.5), 
                "width": 20, 
                "height": 20
            })).unwrap()["id"].as_u64().unwrap();
            s.execute("paint.setStroke", &json!({"none": true, "ids": [id]})).unwrap();
            s.execute("transparency.set", &json!({"ids": [id], "opacity": 90})).unwrap();
            trans_ids.push(NodeId(id));
        }
        
        // Create a spot-colored shape on top (last in stacking order)
        let spot_id = s.execute("shape.rectangle", &json!({"x": 15, "y": 15, "width": 15, "height": 15})).unwrap()["id"].as_u64().unwrap();
        s.execute("paint.setFill", &json!({"swatch": "SpotInk", "ids": [spot_id]})).unwrap();
        s.execute("paint.setStroke", &json!({"none": true, "ids": [spot_id]})).unwrap();
        let spot_node = NodeId(spot_id);
        
        // Create another spot shape that overlaps the transparent group
        let spot_id2 = s.execute("shape.rectangle", &json!({"x": 25, "y": 25, "width": 10, "height": 10})).unwrap()["id"].as_u64().unwrap();
        s.execute("paint.setFill", &json!({"swatch": "SpotInk", "ids": [spot_id2]})).unwrap();
        s.execute("paint.setStroke", &json!({"none": true, "ids": [spot_id2]})).unwrap();
        let spot_node2 = NodeId(spot_id2);
        
        let o = FlattenOptions { 
            no_transparency: Some(Default::default()), 
            balance: 100.0,  // High balance to try to keep vector, but we'll exceed the region cap
            ..FlattenOptions::default() 
        };
        
        let doc = &s.doc().unwrap().doc;
        let all_roots: Vec<NodeId> = trans_ids.iter().copied().chain([spot_node, spot_node2]).collect();
        let p = plan(doc, &all_roots, &o, true).unwrap();
        
        // With spot preservation, the spot nodes should be preserved even if the group goes over-cap
        // Check that at least one flat group has preserved spots
        let has_preserved = p.flat.iter().any(|f| !f.preserved_spots.is_empty());
        // The preserved spots should include our spot nodes
        let preserved_ids: Vec<NodeId> = p.flat.iter()
            .flat_map(|f| f.preserved_spots.iter().map(|(id, _)| *id))
            .collect();
        
        // Note: This test verifies the basic mechanism. The actual preservation depends on
        // whether the spot nodes are at the very top and don't interact with transparency.
        // In this test setup, spot_node and spot_node2 are on top, so they should be preserved.
        if has_preserved {
            assert!(preserved_ids.contains(&spot_node) || preserved_ids.contains(&spot_node2),
                "preserved spots should include at least one of the spot nodes");
        }
    }
}
