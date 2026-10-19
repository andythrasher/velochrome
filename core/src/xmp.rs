//! Ratings, color labels and pick flags in XMP sidecars, the files Lightroom, Bridge and
//! Photo Mechanic also read and write (`P1020943.xmp` beside `P1020943.RW2`).
//!
//! A sidecar that already exists is changed in place: only the text of the values we own is
//! touched, so everything else (Lightroom's develop settings, keywords, layout) stays exactly
//! as it was. That's why this has its own small XML scanner: it records where every tag and
//! attribute sits, which a general XML library's reader/writer round trip doesn't preserve.
//!
//! Stars and labels use the standard `xmp:Rating` and `xmp:Label`. There's no standard field
//! for picks and rejects (Lightroom keeps them only in its catalog), so they go in our own
//! namespace. Bridge's convention of "rating −1 means rejected" is read, but not written, since
//! it would lose the star rating.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::develop::{DETAIL_DEFAULTS, Detail, Edit, MIXER_FIELDS};
use crate::geometry;

pub const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
pub const NS_CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
/// Our namespace. A URN, so it claims no web domain.
pub const NS_VELOCHROME: &str = "urn:velochrome:1.0";
/// The namespace from when the app was called Windedit. Still read, and moved to ours on the next write.
pub const NS_WINDEDIT: &str = "urn:windedit:1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pick {
    #[default]
    Unflagged,
    Picked,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Metadata {
    /// Stars, 0–5.
    pub rating: u8,
    /// Lightroom's label names: Red, Yellow, Green, Blue, Purple.
    pub label: Option<String>,
    pub pick: Pick,
    /// The adjustments, saved in our namespace (`velochrome:Exposure` and so on). Only those
    /// that differ from the photo as shot are written.
    pub edit: Edit,
    /// The local adjustments (docs/phase2d-masks.md), `velochrome:Masks` as one line of text
    /// (`masks::to_text`). None when there are none; never Some of an empty set.
    pub masks: Option<Arc<crate::masks::Masks>>,
    /// The brush strokes of the masks brush components, `velochrome:Strokes` (`masks::strokes_to_text`).
    /// None when there are none.
    pub strokes: Option<crate::masks::Strokes>,
}

/// The sidecar for a photo whose files are named `stem` in `dir`.
pub fn sidecar_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.xmp"))
}

/// Reads a sidecar; no file (or one that can't be understood) means no metadata.
pub fn read(path: &Path) -> Metadata {
    std::fs::read_to_string(path).ok().and_then(|text| parse(&text)).unwrap_or_default()
}

pub fn parse(text: &str) -> Option<Metadata> {
    let mut scan = scan(text).ok()?;
    // A sidecar from the Windedit days has its values in the old namespace; ours win where both are present.
    for (now, old) in scan.props.iter_mut().zip(scan.legacy.iter_mut()) {
        if now.is_none() {
            *now = old.take();
        }
    }
    let mut meta = Metadata::default();
    let rating: i32 = scan.props[RATING].as_ref().and_then(|p| p.value.trim().parse().ok()).unwrap_or(0);
    meta.rating = rating.clamp(0, 5) as u8;
    meta.label = scan.props[LABEL].as_ref().map(|p| p.value.trim().to_string()).filter(|l| !l.is_empty());
    meta.pick = match scan.props[PICK].as_ref().map(|p| p.value.trim().to_string()).as_deref() {
        Some("1") => Pick::Picked,
        Some("-1") => Pick::Rejected,
        Some(_) => Pick::Unflagged,
        None if rating < 0 => Pick::Rejected, // Bridge's reject
        None => Pick::Unflagged,
    };
    let number = |i: usize| scan.props[i].as_ref().and_then(|p| p.value.trim().trim_start_matches('+').parse::<f32>().ok());
    let e = &mut meta.edit;
    for (i, field) in EDIT_FIELDS.iter().enumerate() {
        if let Some(v) = number(FIRST_EDIT + i) {
            *field_mut(e, *field) = v;
        }
    }
    e.custom_white_balance = number(FIRST_EDIT + 1).is_some() as u32;
    e.rotation = number(ROTATION).map_or(0.0, |v| v.rem_euclid(4.0)) as u32;
    e.vignette_amount = number(ROTATION + 1).unwrap_or(0.0);
    e.vignette_midpoint = number(ROTATION + 2).map_or(0.0, |v| v - 50.0);
    // Written as 0 when off; anything else (or nothing) leaves the correction on.
    e.no_distortion = (number(ROTATION + 3) == Some(0.0)) as u32;
    e.no_chromatic_aberration = (number(ROTATION + 4) == Some(0.0)) as u32;
    // The detail sliders are written as they read (Amount 40 by default and so on); missing is the default.
    let mut detail = DETAIL_DEFAULTS;
    for (i, field) in detail_fields_mut(&mut detail).into_iter().enumerate() {
        if let Some(v) = number(DETAIL + i) {
            *field = v;
        }
    }
    e.set_detail(detail);
    e.clarity = number(DETAIL + 9).unwrap_or(0.0);
    for (i, v) in e.mixer.iter_mut().enumerate() {
        *v = number(MIXER + i).unwrap_or(0.0);
    }
    e.profile = if let Some(p) = &scan.props[PROFILE] {
        crate::profile::id_for_name(&p.value)
    } else if let Some(crs) = &scan.crs_profile {
        crate::profile::id_for_lightroom_name(crs)
    } else {
        0
    };
    geometry::clean(e);
    e.clean_lens();
    e.clean_detail();
    e.clean_mixer();
    meta.strokes = scan.props[STROKES].as_ref().map(|p| crate::masks::strokes_from_text(&p.value)).filter(|m| !m.is_empty()).map(Arc::new);
    meta.masks = scan.props[MASKS].as_ref().map(|p| crate::masks::from_text(&p.value)).filter(|m| !m.is_empty()).map(Arc::new);
    Some(meta)
}

/// Writes `meta` to the sidecar: updating it in place if it exists, creating it if not.
/// Written to a temporary file first and swapped in, so a crash can't leave half a file.
pub fn write(path: &Path, meta: &Metadata) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(existing) => update(&existing, meta).map_err(std::io::Error::other)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => new_sidecar(meta),
        Err(e) => return Err(e),
    };
    let temp = path.with_extension("xmp.windedit-tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

/// A fresh sidecar, laid out the way Lightroom lays out its own.
pub fn new_sidecar(meta: &Metadata) -> String {
    let mut attrs = String::new();
    for (i, value) in values(meta, true).into_iter().enumerate() {
        if let Some(value) = value {
            let (_, prefix, local) = PROPS[i];
            attrs.push_str(&format!("\n   {prefix}:{local}=\"{}\"", escape(&value)));
        }
    }
    format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Velochrome\">\n \
         <rdf:RDF xmlns:rdf=\"{NS_RDF}\">\n  \
         <rdf:Description rdf:about=\"\"\n    \
         xmlns:xmp=\"{NS_XMP}\"\n    \
         xmlns:velochrome=\"{NS_VELOCHROME}\"{attrs}/>\n \
         </rdf:RDF>\n</x:xmpmeta>\n"
    )
}

fn pick_value(pick: Pick) -> Option<&'static str> {
    match pick {
        Pick::Unflagged => None,
        Pick::Picked => Some("1"),
        Pick::Rejected => Some("-1"),
    }
}

/// `text` with our values changed to `meta`'s and nothing else touched.
pub fn update(text: &str, meta: &Metadata) -> Result<String, String> {
    let mut scan = scan(text)?;
    let first = scan.descriptions.first().ok_or("no rdf:Description to put the rating in")?;

    // Leave a zero rating out unless the file already has one (Lightroom writes 0).
    let wanted = values(meta, scan.props[RATING].is_some());

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    // Old-namespace values are dropped; whatever they held is written again in ours below.
    for old in scan.legacy.iter_mut() {
        if let Some(old) = old.take() {
            edits.push((old.whole.clone(), String::new()));
        }
    }
    let mut insert = String::new();
    let mut declare: Vec<(&str, &str)> = Vec::new();
    for (i, want) in wanted.iter().enumerate() {
        match (&scan.props[i], want) {
            (Some(found), Some(value)) if found.value != *value => edits.push((found.value_range.clone(), escape(value))),
            (Some(_), Some(_)) => {}
            (Some(found), None) => edits.push((found.whole.clone(), String::new())),
            (None, Some(value)) => {
                let (ns, preferred, local) = PROPS[i];
                let prefix = match first.prefixes.iter().find(|(_, uri)| uri.as_str() == ns) {
                    Some((p, _)) => p.clone(),
                    None => {
                        declare.push((preferred, ns));
                        preferred.to_string()
                    }
                };
                insert.push_str(&format!("\n   {prefix}:{local}=\"{}\"", escape(value)));
            }
            (None, None) => {}
        }
    }
    if !insert.is_empty() {
        let declarations: String = declare.iter().map(|(p, ns)| format!("\n    xmlns:{p}=\"{ns}\"")).collect();
        let at = first.attrs_end;
        edits.push((at..at, declarations + &insert));
    }

    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut out = text.to_string();
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }
    Ok(out)
}

const RATING: usize = 0;
const LABEL: usize = 1;
const PICK: usize = 2;
/// (namespace, the prefix to declare if the file has none for it, local name)
const FIRST_EDIT: usize = 3;
/// (namespace, the prefix to declare if the file has none for it, local name)
const PROPS: [(&str, &str, &str); 36 + MIXER_FIELDS] = [
    (NS_XMP, "xmp", "Rating"),
    (NS_XMP, "xmp", "Label"),
    (NS_VELOCHROME, "velochrome", "Pick"),
    (NS_VELOCHROME, "velochrome", "Exposure"),
    (NS_VELOCHROME, "velochrome", "Temperature"),
    (NS_VELOCHROME, "velochrome", "Tint"),
    (NS_VELOCHROME, "velochrome", "Contrast"),
    (NS_VELOCHROME, "velochrome", "Highlights"),
    (NS_VELOCHROME, "velochrome", "Shadows"),
    (NS_VELOCHROME, "velochrome", "Whites"),
    (NS_VELOCHROME, "velochrome", "Blacks"),
    (NS_VELOCHROME, "velochrome", "Vibrance"),
    (NS_VELOCHROME, "velochrome", "Saturation"),
    (NS_VELOCHROME, "velochrome", "CropAngle"),
    (NS_VELOCHROME, "velochrome", "CropLeft"),
    (NS_VELOCHROME, "velochrome", "CropTop"),
    (NS_VELOCHROME, "velochrome", "CropRight"),
    (NS_VELOCHROME, "velochrome", "CropBottom"),
    (NS_VELOCHROME, "velochrome", "Rotation"),
    (NS_VELOCHROME, "velochrome", "VignetteAmount"),
    (NS_VELOCHROME, "velochrome", "VignetteMidpoint"),
    (NS_VELOCHROME, "velochrome", "LensDistortion"),
    (NS_VELOCHROME, "velochrome", "LensChromaticAberration"),
    (NS_VELOCHROME, "velochrome", "SharpenAmount"),
    (NS_VELOCHROME, "velochrome", "SharpenRadius"),
    (NS_VELOCHROME, "velochrome", "SharpenDetail"),
    (NS_VELOCHROME, "velochrome", "SharpenMasking"),
    (NS_VELOCHROME, "velochrome", "NoiseLuminance"),
    (NS_VELOCHROME, "velochrome", "NoiseLuminanceDetail"),
    (NS_VELOCHROME, "velochrome", "NoiseColor"),
    (NS_VELOCHROME, "velochrome", "NoiseColorDetail"),
    (NS_VELOCHROME, "velochrome", "Texture"),
    (NS_VELOCHROME, "velochrome", "Clarity"),
    (NS_VELOCHROME, "velochrome", "HueRed"),
    (NS_VELOCHROME, "velochrome", "HueOrange"),
    (NS_VELOCHROME, "velochrome", "HueYellow"),
    (NS_VELOCHROME, "velochrome", "HueGreen"),
    (NS_VELOCHROME, "velochrome", "HueAqua"),
    (NS_VELOCHROME, "velochrome", "HueBlue"),
    (NS_VELOCHROME, "velochrome", "HuePurple"),
    (NS_VELOCHROME, "velochrome", "HueMagenta"),
    (NS_VELOCHROME, "velochrome", "SaturationRed"),
    (NS_VELOCHROME, "velochrome", "SaturationOrange"),
    (NS_VELOCHROME, "velochrome", "SaturationYellow"),
    (NS_VELOCHROME, "velochrome", "SaturationGreen"),
    (NS_VELOCHROME, "velochrome", "SaturationAqua"),
    (NS_VELOCHROME, "velochrome", "SaturationBlue"),
    (NS_VELOCHROME, "velochrome", "SaturationPurple"),
    (NS_VELOCHROME, "velochrome", "SaturationMagenta"),
    (NS_VELOCHROME, "velochrome", "LuminanceRed"),
    (NS_VELOCHROME, "velochrome", "LuminanceOrange"),
    (NS_VELOCHROME, "velochrome", "LuminanceYellow"),
    (NS_VELOCHROME, "velochrome", "LuminanceGreen"),
    (NS_VELOCHROME, "velochrome", "LuminanceAqua"),
    (NS_VELOCHROME, "velochrome", "LuminanceBlue"),
    (NS_VELOCHROME, "velochrome", "LuminancePurple"),
    (NS_VELOCHROME, "velochrome", "LuminanceMagenta"),
    (NS_VELOCHROME, "velochrome", "Masks"),
    (NS_VELOCHROME, "velochrome", "Strokes"),
    (NS_VELOCHROME, "velochrome", "Profile"),
];

/// The edit's fields, in PROPS order from FIRST_EDIT.
#[derive(Clone, Copy)]
enum EditField {
    Exposure,
    Temperature,
    Tint,
    Contrast,
    Highlights,
    Shadows,
    Whites,
    Blacks,
    Vibrance,
    Saturation,
    Angle,
    CropLeft,
    CropTop,
    CropRight,
    CropBottom,
}

const EDIT_FIELDS: [EditField; 15] = [
    EditField::Exposure,
    EditField::Temperature,
    EditField::Tint,
    EditField::Contrast,
    EditField::Highlights,
    EditField::Shadows,
    EditField::Whites,
    EditField::Blacks,
    EditField::Vibrance,
    EditField::Saturation,
    EditField::Angle,
    EditField::CropLeft,
    EditField::CropTop,
    EditField::CropRight,
    EditField::CropBottom,
];

/// Where Rotation (a whole number, not one of the `f32` fields) sits in PROPS.
const ROTATION: usize = FIRST_EDIT + EDIT_FIELDS.len();

fn field_mut(e: &mut Edit, f: EditField) -> &mut f32 {
    match f {
        EditField::Exposure => &mut e.exposure,
        EditField::Temperature => &mut e.temperature,
        EditField::Tint => &mut e.tint,
        EditField::Contrast => &mut e.contrast,
        EditField::Highlights => &mut e.highlights,
        EditField::Shadows => &mut e.shadows,
        EditField::Whites => &mut e.whites,
        EditField::Blacks => &mut e.blacks,
        EditField::Vibrance => &mut e.vibrance,
        EditField::Saturation => &mut e.saturation,
        EditField::Angle => &mut e.angle,
        EditField::CropLeft => &mut e.crop[0],
        EditField::CropTop => &mut e.crop[1],
        EditField::CropRight => &mut e.crop[2],
        EditField::CropBottom => &mut e.crop[3],
    }
}

/// Each property's text, in PROPS order; None where it shouldn't be in the file. A zero rating
/// is written only if `keep_zero_rating`; adjustments only where they differ from as shot (and
/// temperature and tint only for a custom white balance).
fn values(meta: &Metadata, keep_zero_rating: bool) -> Vec<Option<String>> {
    let mut v = vec![
        (meta.rating > 0 || keep_zero_rating).then(|| meta.rating.to_string()),
        meta.label.clone(),
        pick_value(meta.pick).map(str::to_string),
    ];
    let mut e = meta.edit;
    let mut none = Edit::default();
    for field in EDIT_FIELDS {
        let white_balance = matches!(field, EditField::Temperature | EditField::Tint);
        let value = *field_mut(&mut e, field);
        let crop = matches!(field, EditField::CropLeft | EditField::CropTop | EditField::CropRight | EditField::CropBottom);
        let text = number_text(value, if crop { 5 } else { 2 });
        let written = if white_balance {
            meta.edit.custom_white_balance != 0
        } else {
            text.parse::<f32>().ok() != Some(*field_mut(&mut none, field))
        };
        v.push(written.then_some(text));
    }
    v.push((e.rotation != 0).then(|| e.rotation.to_string()));
    let amount = number_text(e.vignette_amount, 2);
    v.push((amount != "0").then_some(amount));
    // The midpoint is written as the slider shows it (0–100, 50 by default).
    let midpoint = number_text(50.0 + e.vignette_midpoint, 2);
    v.push((midpoint != "50").then_some(midpoint));
    v.push((e.no_distortion != 0).then(|| "0".to_string()));
    v.push((e.no_chromatic_aberration != 0).then(|| "0".to_string()));
    let (mut now, mut default) = (e.detail(), DETAIL_DEFAULTS);
    for (now, default) in detail_fields_mut(&mut now).into_iter().zip(detail_fields_mut(&mut default)) {
        let text = number_text(*now, 2);
        v.push((text.parse::<f32>().ok() != Some(*default)).then_some(text));
    }
    let clarity = number_text(e.clarity, 2);
    v.push((clarity != "0").then_some(clarity));
    for &m in &e.mixer {
        let text = number_text(m, 2);
        v.push((text != "0").then_some(text));
    }
    v.push(meta.masks.as_deref().filter(|m| !m.is_empty()).map(crate::masks::to_text));
    v.push(meta.strokes.as_deref().filter(|m| !m.is_empty()).map(crate::masks::strokes_to_text));
    v.push(crate::profile::name_of(e.profile));
    v
}

/// Where the detail sliders start in PROPS, after the lens fields.
const DETAIL: usize = ROTATION + 5;

/// Where the colour mixer sliders start in PROPS (after Texture and Clarity): hue, saturation and
/// luminance for each range.
const MIXER: usize = DETAIL + 10;

/// Where the masks' text sits in PROPS: the last.
const MASKS: usize = MIXER + MIXER_FIELDS;

/// Where the brush strokes sit in PROPS: after the masks.
const STROKES: usize = MASKS + 1;

/// Where the camera profile name sits in PROPS: after the strokes.
const PROFILE: usize = STROKES + 1;

/// The detail sliders, in PROPS order from DETAIL.
fn detail_fields_mut(d: &mut Detail) -> [&mut f32; 9] {
    [
        &mut d.amount,
        &mut d.radius,
        &mut d.detail,
        &mut d.masking,
        &mut d.luminance,
        &mut d.luminance_detail,
        &mut d.color,
        &mut d.color_detail,
        &mut d.texture,
    ]
}

/// A number as short as it can be: `decimals` at most (two for the sliders, five for crop edges),
/// no trailing zeros.
fn number_text(v: f32, decimals: i32) -> String {
    let scale = 10f32.powi(decimals);
    let rounded = (v * scale).round() / scale;
    if rounded == 0.0 { "0".into() } else { rounded.to_string() }
}

/// One of our values, found in the file.
#[derive(Debug)]
struct Found {
    value: String,
    /// The text to replace to change it.
    value_range: Range<usize>,
    /// The text to remove to delete it, with the whitespace before it.
    whole: Range<usize>,
}

struct Description {
    /// Namespace prefixes in scope at the tag (prefix → URI).
    prefixes: HashMap<String, String>,
    /// Where new attributes go: just before the tag's `>` or `/>`.
    attrs_end: usize,
}

struct Scan {
    /// The top-level rdf:Descriptions (children of rdf:RDF); nested ones, like those inside
    /// Lightroom's crs:Preset, are left alone.
    descriptions: Vec<Description>,
    props: Vec<Option<Found>>,
    /// The same values found under the old windedit: namespace.
    legacy: Vec<Option<Found>>,
    crs_profile: Option<String>,
}

fn scan(text: &str) -> Result<Scan, String> {
    let tokens = tokenize(text)?;
    let mut scopes: Vec<HashMap<String, String>> = vec![HashMap::new()];
    // (namespace, local name, index of the top-level Description it's in, if any)
    let mut stack: Vec<(String, String, Option<usize>)> = Vec::new();
    let mut result = Scan { descriptions: Vec::new(), props: PROPS.iter().map(|_| None).collect(), legacy: PROPS.iter().map(|_| None).collect(), crs_profile: None };
    // An element-form value being read: (prop, start of its whitespace, start of content).
    let mut open_prop: Option<(usize, bool, usize, usize, usize)> = None;

    for token in &tokens {
        match token {
            Token::Start { name, attrs, range, attrs_end, empty } => {
                let mut scope = scopes.last().unwrap().clone();
                for a in attrs {
                    if let Some(prefix) = a.name.strip_prefix("xmlns:") {
                        scope.insert(prefix.to_string(), a.value.clone());
                    } else if a.name == "xmlns" {
                        scope.insert(String::new(), a.value.clone());
                    }
                }
                let (ns, local) = resolve(name, &scope, true);
                let parent = stack.last();
                let parent_is_rdf = parent.is_some_and(|(pns, plocal, _)| pns == NS_RDF && plocal == "RDF");
                let in_description = parent.and_then(|p| p.2);
                let mut own_description = None;

                if ns == NS_RDF && local == "Description" && parent_is_rdf {
                    own_description = Some(result.descriptions.len());
                    result.descriptions.push(Description { prefixes: scope.clone(), attrs_end: *attrs_end });
                    for a in attrs.iter().filter(|a| !a.name.starts_with("xmlns")) {
                        let (ans, alocal) = resolve(&a.name, &scope, false);
                        if let Some((i, old)) = prop_slot(&ans, &alocal)
                            && result.slot(i, old).is_none()
                        {
                            *result.slot(i, old) =
                                Some(Found { value: a.value.clone(), value_range: a.value_range.clone(), whole: a.whole.clone() });
                        } else if ans == NS_CRS && alocal == "CameraProfile" && result.crs_profile.is_none() {
                            result.crs_profile = Some(a.value.clone());
                        }
                    }
                } else if parent.is_some_and(|p| p.2.is_some() && is_description(p))
                    && let Some((i, old)) = prop_slot(&ns, &local)
                    && result.slot(i, old).is_none()
                {
                    // Element form, e.g. <xmp:Rating>3</xmp:Rating>, directly in a Description.
                    let ws_start = text[..range.start].trim_end_matches([' ', '\t', '\r', '\n']).len();
                    if *empty {
                        *result.slot(i, old) =
                            Some(Found { value: String::new(), value_range: range.clone(), whole: ws_start..range.end });
                    } else {
                        open_prop = Some((i, old, ws_start, range.end, stack.len()));
                    }
                }

                if !empty {
                    stack.push((ns, local, own_description.or(in_description)));
                    scopes.push(scope);
                }
            }
            Token::End { range } => {
                if let Some((i, old, ws_start, content_start, depth)) = open_prop
                    && stack.len() == depth + 1
                {
                    let content = content_start..range.start;
                    *result.slot(i, old) = Some(Found {
                        value: unescape(&text[content.clone()]),
                        value_range: content,
                        whole: ws_start..range.end,
                    });
                    open_prop = None;
                }
                stack.pop();
                scopes.pop();
            }
        }
    }
    Ok(result)
}

fn is_description(entry: &(String, String, Option<usize>)) -> bool {
    entry.0 == NS_RDF && entry.1 == "Description"
}

impl Scan {
    fn slot(&mut self, i: usize, old: bool) -> &mut Option<Found> {
        if old { &mut self.legacy[i] } else { &mut self.props[i] }
    }
}

/// The index of the property for this name, and whether it's in the old Windedit namespace.
fn prop_slot(ns: &str, local: &str) -> Option<(usize, bool)> {
    let old = ns == NS_WINDEDIT;
    let ns = if old { NS_VELOCHROME } else { ns };
    PROPS.iter().position(|&(pns, _, plocal)| pns == ns && plocal == local).map(|i| (i, old))
}

/// (namespace URI, local name). Unprefixed attributes have no namespace; unprefixed elements
/// take the default namespace.
fn resolve(name: &str, scope: &HashMap<String, String>, element: bool) -> (String, String) {
    match name.split_once(':') {
        Some((prefix, local)) => (scope.get(prefix).cloned().unwrap_or_default(), local.to_string()),
        None if element => (scope.get("").cloned().unwrap_or_default(), name.to_string()),
        None => (String::new(), name.to_string()),
    }
}

struct Attr {
    name: String,
    value: String,
    value_range: Range<usize>,
    /// From the whitespace before the name to the closing quote.
    whole: Range<usize>,
}

enum Token {
    Start { name: String, attrs: Vec<Attr>, range: Range<usize>, attrs_end: usize, empty: bool },
    End { range: Range<usize> },
}

/// The tags of an XML document with their positions. Text, comments, processing instructions
/// (`<?xpacket ...?>`), CDATA and declarations are skipped.
fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let b = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let find = |from: usize, pat: &str| text[from..].find(pat).map(|p| from + p);
    while let Some(lt) = find(i, "<") {
        let rest = &text[lt..];
        if rest.starts_with("<?") {
            i = find(lt, "?>").ok_or("unterminated <?")? + 2;
        } else if rest.starts_with("<!--") {
            i = find(lt, "-->").ok_or("unterminated comment")? + 3;
        } else if rest.starts_with("<![CDATA[") {
            i = find(lt, "]]>").ok_or("unterminated CDATA")? + 3;
        } else if rest.starts_with("<!") {
            i = find(lt, ">").ok_or("unterminated declaration")? + 1;
        } else if rest.starts_with("</") {
            let gt = find(lt, ">").ok_or("unterminated end tag")?;
            tokens.push(Token::End { range: lt..gt + 1 });
            i = gt + 1;
        } else {
            let mut j = lt + 1;
            while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                j += 1;
            }
            let name = text[lt + 1..j].to_string();
            let mut attrs = Vec::new();
            loop {
                let ws = j;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= b.len() {
                    return Err("unterminated tag".into());
                }
                if b[j] == b'>' || (b[j] == b'/' && b.get(j + 1) == Some(&b'>')) {
                    let empty = b[j] == b'/';
                    let end = if empty { j + 2 } else { j + 1 };
                    tokens.push(Token::Start { name, attrs, range: lt..end, attrs_end: ws, empty });
                    i = end;
                    break;
                }
                let name_start = j;
                while j < b.len() && b[j] != b'=' && !b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let attr_name = text[name_start..j].to_string();
                while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'=') {
                    j += 1;
                }
                let quote = *b.get(j).ok_or("unterminated attribute")?;
                if quote != b'"' && quote != b'\'' {
                    return Err(format!("unquoted attribute {attr_name}"));
                }
                let value_start = j + 1;
                let value_end = text[value_start..].find(quote as char).ok_or("unterminated attribute value")? + value_start;
                attrs.push(Attr {
                    name: attr_name,
                    value: unescape(&text[value_start..value_end]),
                    value_range: value_start..value_end,
                    whole: ws..value_end + 1,
                });
                j = value_end + 1;
            }
        }
    }
    Ok(tokens)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=semi]),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like Lightroom's sidecars, including a nested Description inside crs:Preset that
    /// has an xmp:Rating of its own, which must be left alone.
    const LIGHTROOM: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00        ">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:tiff="http://ns.adobe.com/tiff/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
   xmp:Rating="0"
   xmp:ModifyDate="2026-05-24T17:19:11.036-06:00"
   tiff:Make="Panasonic"
   crs:Exposure2012="+0.35">
   <crs:Preset>
    <rdf:Description
     xmp:Rating="9"
     crs:Name="Nested"/>
   </crs:Preset>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

    fn changed_lines(before: &str, after: &str) -> Vec<String> {
        let old: Vec<&str> = before.lines().collect();
        after.lines().filter(|l| !old.contains(l)).map(str::to_string).collect()
    }

    #[test]
    fn reads_lightroom() {
        assert_eq!(parse(LIGHTROOM), Some(Metadata::default()));
    }

    #[test]
    fn updates_lightroom_in_place() {
        let meta = Metadata { rating: 4, label: Some("Red".into()), pick: Pick::Picked, ..Default::default() };
        let out = update(LIGHTROOM, &meta).unwrap();
        assert_eq!(parse(&out), Some(meta));
        // Only our values changed or appeared; every other line is as it was.
        let mut changed = changed_lines(LIGHTROOM, &out);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "    xmlns:velochrome=\"urn:velochrome:1.0\"".to_string(),
                "   crs:Exposure2012=\"+0.35\"".to_string(), // the tag's `>` moved after our new attributes
                "   velochrome:Pick=\"1\">".to_string(),
                "   xmp:Label=\"Red\"".to_string(),
                "   xmp:Rating=\"4\"".to_string(),
            ]
        );
        assert!(out.contains("xmp:Rating=\"9\""), "the nested Description is untouched");
        assert_eq!(out.lines().count(), LIGHTROOM.lines().count() + 3);

        // And back: clearing them restores the original, apart from our (now unused) namespace
        // declaration, which is harmless and left in place.
        let cleared = update(&out, &Metadata::default()).unwrap();
        let expected = LIGHTROOM.replace(
            "crs:Exposure2012=\"+0.35\">",
            "crs:Exposure2012=\"+0.35\"
    xmlns:velochrome=\"urn:velochrome:1.0\">",
        );
        assert_eq!(cleared, expected);
    }

    #[test]
    fn element_form_and_old_prefix() {
        let old = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:xap="http://ns.adobe.com/xap/1.0/">
 <xap:Rating>2</xap:Rating>
 <xap:Label>Blue &amp; green</xap:Label>
</rdf:Description></rdf:RDF></x:xmpmeta>
<?xpacket end="w"?>"#;
        let meta = parse(old).unwrap();
        assert_eq!(meta.rating, 2);
        assert_eq!(meta.label.as_deref(), Some("Blue & green"));

        let out = update(old, &Metadata { rating: 5, ..Default::default() }).unwrap();
        assert!(out.contains("<xap:Rating>5</xap:Rating>"));
        assert!(!out.contains("Label"));
        assert!(out.starts_with("<?xpacket") && out.ends_with("<?xpacket end=\"w\"?>"));
    }

    #[test]
    fn edits_round_trip() {
        let edit = Edit {
            exposure: 0.35,
            custom_white_balance: 1,
            temperature: 5250.0,
            tint: -7.5,
            shadows: 40.0,
            vibrance: 15.0,
            ..Default::default()
        };
        let meta = Metadata { rating: 2, edit, ..Default::default() };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("velochrome:Exposure=\"0.35\"") && !fresh.contains("Contrast"), "{fresh}");
        assert_eq!(parse(&fresh), Some(meta.clone()));
        // Into Lightroom's file too, and back out again: only defaults left, nothing written.
        let out = update(LIGHTROOM, &meta).unwrap();
        assert_eq!(parse(&out), Some(meta));
        let back = update(&out, &Metadata { rating: 2, ..Default::default() }).unwrap();
        assert!(!back.contains("velochrome:Exposure") && !back.contains("velochrome:Temperature"));
    }

    #[test]
    fn geometry_round_trips_and_defaults_are_not_written() {
        let edit = Edit { rotation: 3, angle: -2.35, crop: [0.0125, 0.1, 0.9, 0.87654], ..Default::default() };
        let meta = Metadata { edit, ..Default::default() };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("velochrome:Rotation=\"3\"") && fresh.contains("velochrome:CropAngle=\"-2.35\""), "{fresh}");
        assert!(fresh.contains("velochrome:CropLeft=\"0.0125\"") && fresh.contains("velochrome:CropBottom=\"0.87654\""), "{fresh}");
        // The default edges are left out: only the left, top and bottom here.
        assert!(!fresh.contains("CropRight=\"1\""));
        assert_eq!(parse(&fresh), Some(meta.clone()));
        let out = update(LIGHTROOM, &meta).unwrap();
        assert_eq!(parse(&out), Some(meta));
        // A photo with no geometry has none of it in the file, and reads back as uncropped.
        let plain = new_sidecar(&Metadata { rating: 1, ..Default::default() });
        assert!(!plain.contains("Crop") && !plain.contains("Rotation"));
        let read = parse(&plain).unwrap().edit;
        assert_eq!(read.crop, [0.0, 0.0, 1.0, 1.0]);
        assert!(read.is_default());
        // A damaged crop reads as none.
        let bad = out.replace("velochrome:CropLeft=\"0.0125\"", "velochrome:CropLeft=\"0.95\"");
        assert_eq!(parse(&bad).unwrap().edit.crop, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn lens_fields_round_trip_and_defaults_are_not_written() {
        let edit = Edit { vignette_amount: -35.5, vignette_midpoint: 20.0, no_distortion: 1, no_chromatic_aberration: 1, ..Default::default() };
        let meta = Metadata { edit, ..Default::default() };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("velochrome:VignetteAmount=\"-35.5\"") && fresh.contains("velochrome:VignetteMidpoint=\"70\""), "{fresh}");
        assert!(fresh.contains("velochrome:LensDistortion=\"0\"") && fresh.contains("velochrome:LensChromaticAberration=\"0\""), "{fresh}");
        assert_eq!(parse(&fresh), Some(meta.clone()));
        let out = update(LIGHTROOM, &meta).unwrap();
        assert_eq!(parse(&out), Some(meta));
        // Each field on its own, and the defaults leave nothing in the file.
        for one in [
            Edit { vignette_amount: 100.0, ..Default::default() },
            Edit { vignette_midpoint: -50.0, vignette_amount: 1.0, ..Default::default() },
            Edit { no_chromatic_aberration: 1, ..Default::default() },
        ] {
            let m = Metadata { edit: one, ..Default::default() };
            assert_eq!(parse(&new_sidecar(&m)), Some(m));
        }
        let plain = new_sidecar(&Metadata { rating: 1, ..Default::default() });
        assert!(!plain.contains("Vignette") && !plain.contains("Lens"), "{plain}");
        let read = parse(&plain).unwrap().edit;
        assert!(read.is_default() && read.no_distortion == 0 && read.vignette_midpoint == 0.0);
        // Damage is put in range.
        let bad = out.replace("VignetteAmount=\"-35.5\"", "VignetteAmount=\"900\"");
        assert_eq!(parse(&bad).unwrap().edit.vignette_amount, 100.0);
        // Back to the defaults takes them out of the file.
        let back = update(&out, &Metadata::default()).unwrap();
        assert!(!back.contains("velochrome:Vignette") && !back.contains("velochrome:Lens"));
    }

    #[test]
    fn detail_fields_round_trip_and_defaults_are_not_written() {
        let mut edit = Edit { clarity: 60.0, ..Default::default() };
        edit.set_detail(Detail {
            amount: 85.0,
            radius: 1.6,
            detail: 60.0,
            masking: 35.0,
            luminance: 42.0,
            luminance_detail: 20.0,
            color: 0.0,
            color_detail: 75.0,
            texture: -35.0,
        });
        let meta = Metadata { edit, ..Default::default() };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("velochrome:SharpenAmount=\"85\"") && fresh.contains("velochrome:NoiseColor=\"0\""), "{fresh}");
        assert!(fresh.contains("velochrome:SharpenRadius=\"1.6\""), "{fresh}");
        assert!(fresh.contains("velochrome:Texture=\"-35\"") && fresh.contains("velochrome:Clarity=\"60\""), "{fresh}");
        assert_eq!(parse(&fresh).unwrap().edit.clarity, 60.0);
        assert_eq!(parse(&fresh).unwrap().edit.detail(), meta.edit.detail());
        let out = update(LIGHTROOM, &meta).unwrap();
        assert_eq!(parse(&out).unwrap().edit.detail(), meta.edit.detail());
        // Each on its own, including a value on the far side of the default.
        for (i, one) in [
            Edit { sharpen_amount: -40.0, ..Default::default() },
            Edit { sharpen_radius: 2.0, ..Default::default() },
            Edit { noise_luminance_detail: -50.0, ..Default::default() },
            Edit { noise_color: 75.0, ..Default::default() },
            Edit { texture: 100.0, ..Default::default() },
            Edit { clarity: -100.0, ..Default::default() },
        ]
        .into_iter()
        .enumerate()
        {
            let m = Metadata { edit: one, ..Default::default() };
            assert_eq!(parse(&new_sidecar(&m)), Some(m), "case {i}");
        }
        // The defaults leave nothing in the file, and a file without them reads as the defaults.
        let plain = new_sidecar(&Metadata { rating: 1, ..Default::default() });
        assert!(!plain.contains("Sharpen") && !plain.contains("Noise") && !plain.contains("Texture") && !plain.contains("Clarity"), "{plain}");
        let read = parse(&plain).unwrap().edit;
        assert!(read.is_default() && read.detail() == DETAIL_DEFAULTS);
        // Damage is put in range.
        let bad = fresh.replace("SharpenAmount=\"85\"", "SharpenAmount=\"9000\"").replace("NoiseColorDetail=\"75\"", "NoiseColorDetail=\"x\"");
        let d = parse(&bad).unwrap().edit.detail();
        assert_eq!((d.amount, d.color_detail), (150.0, 50.0));
        // Back to the defaults takes them out of the file.
        let back = update(&out, &Metadata::default()).unwrap();
        assert!(!back.contains("velochrome:Sharpen") && !back.contains("velochrome:Noise"));
    }

    #[test]
    fn mixer_fields_round_trip_and_zeros_are_not_written() {
        let mut edit = Edit::default();
        for (i, v) in edit.mixer.iter_mut().enumerate() {
            *v = [0.0, 37.5, -100.0, 100.0, -1.0][i % 5];
        }
        let meta = Metadata { edit, ..Default::default() };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("velochrome:HueOrange=\"37.5\"") && fresh.contains("velochrome:HueYellow=\"-100\""), "{fresh}");
        assert!(fresh.contains("velochrome:SaturationRed=\"37.5\"") || fresh.contains("velochrome:SaturationRed=\"-1\"") || fresh.contains("velochrome:SaturationRed=\"100\""), "{fresh}");
        assert!(!fresh.contains("velochrome:HueRed"), "zero is not written: {fresh}");
        assert_eq!(parse(&fresh), Some(meta.clone()));
        let out = update(LIGHTROOM, &meta).unwrap();
        assert_eq!(parse(&out), Some(meta));
        // Each of the 24 on its own.
        for i in 0..MIXER_FIELDS {
            let mut one = Edit::default();
            one.mixer[i] = if i % 2 == 0 { -42.0 } else { 71.0 };
            let m = Metadata { edit: one, ..Default::default() };
            let text = new_sidecar(&m);
            assert_eq!(text.matches("velochrome:").count() - 1, 1, "only one value: {text}"); // the namespace declaration is the other
            assert_eq!(parse(&text), Some(m), "slider {i}");
        }
        // Zeros leave nothing in the file; a file without them reads as zeros.
        let plain = new_sidecar(&Metadata { rating: 1, ..Default::default() });
        assert!(!plain.contains("Hue") && !plain.contains("Saturation") && !plain.contains("Luminance"), "{plain}");
        assert!(parse(&plain).unwrap().edit.mixer == [0.0; MIXER_FIELDS]);
        // Damage is put in range.
        let bad = fresh.replace("HueOrange=\"37.5\"", "HueOrange=\"900\"").replace("HueYellow=\"-100\"", "HueYellow=\"x\"");
        let e = parse(&bad).unwrap().edit;
        assert_eq!((e.mixer[1], e.mixer[2]), (100.0, 0.0));
        // Back to zero takes them out of the file.
        let back = update(&out, &Metadata::default()).unwrap();
        assert!(!back.contains("velochrome:Hue") && !back.contains("velochrome:Saturation") && !back.contains("velochrome:Luminance"));
    }

    #[test]
    fn masks_round_trip_and_none_is_not_written() {
        use crate::masks::{ADD, Component, KIND_LINEAR, KIND_RADIAL, Mask, Masks, SUBTRACT};
        let mut masks = Masks::default();
        let mut m = Mask::new();
        m.amount = 70.0;
        m.sliders[2] = -0.75;
        m.sliders[3] = 30.0;
        m.components[0] = Component { kind: KIND_LINEAR, mode: ADD, p: [0.1, 0.1, 0.4, 0.5, 0.0, 0.0, 0.0, 0.0] };
        m.components[1] = Component { kind: KIND_RADIAL, mode: SUBTRACT, p: [0.5, 0.3, 0.2, 0.1, 0.4, 25.0, 1.0, 0.0] };
        m.count = 2;
        masks.masks[0] = m;
        masks.count = 1;
        let meta = Metadata { rating: 1, edit: Edit { exposure: 0.5, ..Default::default() }, masks: Some(Arc::new(masks)), ..Default::default() };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("velochrome:Masks=\"amount=70 exposure=-0.75 contrast=30 | L+ 0.1 0.1 0.4 0.5 | R- 0.5 0.3 0.2 0.1 0.4 25 1\""), "{fresh}");
        assert_eq!(parse(&fresh), Some(meta.clone()));
        // Into Lightroom's file, and back out: gone again, with everything else of ours.
        let out = update(LIGHTROOM, &meta).unwrap();
        assert_eq!(parse(&out), Some(meta.clone()));
        let back = update(&out, &Metadata { rating: 1, ..Default::default() }).unwrap();
        assert!(!back.contains("velochrome:Masks") && !back.contains("velochrome:Exposure"), "{back}");
        // Masks alone make an edited photo's sidecar; no masks write nothing, and damage reads as none.
        let plain = new_sidecar(&Metadata { rating: 1, ..Default::default() });
        assert!(!plain.contains("Masks") && parse(&plain).unwrap().masks.is_none());
        let bad = fresh.replace("velochrome:Masks=\"", "velochrome:Masks=\"zz | ");
        assert_eq!(parse(&bad).unwrap().masks.unwrap().masks[0].count, 2);
        assert!(parse(&fresh.replace("amount=70 exposure=-0.75 contrast=30 | L+ 0.1 0.1 0.4 0.5 | R- 0.5 0.3 0.2 0.1 0.4 25 1", "")).unwrap().masks.is_none());
    }

    #[test]
    fn bridge_reject_is_read() {
        let bridge = LIGHTROOM.replace("xmp:Rating=\"0\"", "xmp:Rating=\"-1\"");
        assert_eq!(parse(&bridge).unwrap().pick, Pick::Rejected);
    }

    #[test]
    fn new_sidecar_round_trips() {
        let meta = Metadata { rating: 3, label: Some("Green".into()), pick: Pick::Rejected, ..Default::default() };
        assert_eq!(parse(&new_sidecar(&meta)), Some(meta.clone()));
        let path = std::env::temp_dir().join(format!("velochrome-xmp-test-{}.xmp", std::process::id()));
        let _ = std::fs::remove_file(&path);
        write(&path, &meta).unwrap();
        assert_eq!(read(&path), meta);
        write(&path, &Metadata { rating: 1, ..meta.clone() }).unwrap();
        assert_eq!(read(&path).rating, 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn profile_round_trips_and_reads_lightroom_crs() {
        let id = crate::profile::id_for_name("Camera Natural");
        let meta = Metadata { edit: Edit { profile: id, ..Default::default() }, ..Default::default() };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("velochrome:Profile=\"Camera Natural\""), "{fresh}");
        assert_eq!(parse(&fresh), Some(meta.clone()));

        // Default profile (0) writes nothing.
        let plain = new_sidecar(&Metadata::default());
        assert!(!plain.contains("Profile"));

        // Lightroom's crs:CameraProfile="Adobe Color" stays 0; "Camera Vivid" is picked up.
        let lr_color = LIGHTROOM.replace("crs:Exposure2012=\"+0.35\"", "crs:CameraProfile=\"Adobe Color\"");
        assert_eq!(parse(&lr_color).unwrap().edit.profile, 0);
        let lr_vivid = LIGHTROOM.replace("crs:Exposure2012=\"+0.35\"", "crs:CameraProfile=\"Camera Vivid\"");
        assert_eq!(parse(&lr_vivid).unwrap().edit.profile, crate::profile::id_for_name("Camera Vivid"));
    }

    /// A sidecar as Windedit wrote it, with Lightroom's own settings beside our values.
    const OLD_WINDEDIT: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Windedit">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:windedit="urn:windedit:1.0"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
   xmp:Rating="3"
   crs:Saturation="+5"
   windedit:Exposure="0.5"
   windedit:Contrast="12"
   windedit:Masks="amount=70 exposure=-0.75 contrast=30 | L+ 0.1 0.1 0.4 0.5"
   windedit:Profile="Camera Natural"/>
 </rdf:RDF>
</x:xmpmeta>
"#;

    #[test]
    fn old_windedit_sidecar_is_read_and_migrated() {
        let meta = parse(OLD_WINDEDIT).unwrap();
        assert_eq!(meta.rating, 3);
        assert_eq!(meta.edit.exposure, 0.5);
        assert_eq!(meta.edit.contrast, 12.0);
        assert!(meta.masks.is_some());
        assert_eq!(meta.edit.profile, crate::profile::id_for_name("Camera Natural"));

        // Rewriting moves every field to the new namespace and keeps Lightroom's.
        let out = update(OLD_WINDEDIT, &meta).unwrap();
        assert!(!out.contains("windedit:Exposure") && !out.contains("windedit:Masks") && !out.contains("windedit:Profile"), "{out}");
        assert!(out.contains("velochrome:Exposure=\"0.5\"") && out.contains("velochrome:Contrast=\"12\""), "{out}");
        assert!(out.contains("crs:Saturation=\"+5\"") && out.contains("xmp:Rating=\"3\""), "{out}");
        assert_eq!(parse(&out), Some(meta.clone()));

        // A value cleared in the meantime is gone from the file, not left behind in the old namespace.
        let cleared = update(OLD_WINDEDIT, &Metadata { rating: 3, ..Default::default() }).unwrap();
        assert!(!cleared.contains("windedit:Exposure") && !cleared.contains("velochrome:Exposure"), "{cleared}");
        assert_eq!(parse(&cleared).unwrap().edit, Edit::default());
    }

    #[test]
    fn new_velochrome_sidecar_round_trips() {
        let meta = Metadata {
            rating: 2,
            pick: Pick::Picked,
            edit: Edit { exposure: -0.4, shadows: 25.0, ..Default::default() },
            ..Default::default()
        };
        let fresh = new_sidecar(&meta);
        assert!(fresh.contains("xmlns:velochrome=\"urn:velochrome:1.0\"") && !fresh.to_lowercase().contains("windedit"), "{fresh}");
        assert_eq!(parse(&fresh), Some(meta.clone()));
        let again = update(&fresh, &Metadata { edit: Edit { exposure: 0.2, ..meta.edit }, ..meta.clone() }).unwrap();
        assert!(again.contains("velochrome:Exposure=\"0.2\""), "{again}");
        assert_eq!(parse(&again).unwrap().edit.exposure, 0.2);
    }

    #[test]
    fn mixed_sidecar_prefers_the_new_namespace_and_drops_the_old() {
        // Both namespaces, as if an old build had touched a newer file: the new value wins.
        let mixed = OLD_WINDEDIT.replace(
            "   windedit:Exposure=\"0.5\"",
            "   xmlns:velochrome=\"urn:velochrome:1.0\"\n   velochrome:Exposure=\"0.9\"\n   windedit:Exposure=\"0.5\"",
        );
        let meta = parse(&mixed).unwrap();
        assert_eq!(meta.edit.exposure, 0.9);
        assert_eq!(meta.edit.contrast, 12.0, "still read from the old namespace");
        let out = update(&mixed, &meta).unwrap();
        assert!(!out.contains("windedit:Exposure") && !out.contains("windedit:Contrast"), "{out}");
        assert_eq!(out.matches("velochrome:Exposure").count(), 1, "{out}");
        assert!(out.contains("velochrome:Exposure=\"0.9\"") && out.contains("velochrome:Contrast=\"12\""), "{out}");
        assert_eq!(parse(&out), Some(meta));
    }
}
