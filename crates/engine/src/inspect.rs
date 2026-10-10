//! Embed build: compare a saved PDF with the file it was edited from. A host checks every save from
//! the embed build with this. Facts only; what is acceptable is the host's rule.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use pdfcraft_render::{Annotation, DocInfo};

use crate::Session;

/// A longer document is refused rather than compared slowly.
pub const MAX_PAGES: usize = 2_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspection {
    pub encrypted: bool,
    /// The edited file starts with the base's bytes: the edit is an appended update.
    pub base_kept: bool,
    pub pages: PageChanges,
    pub form_fields_changed: u32,
    pub annotations: AnnotationChanges,
    pub text: TextChanges,
    pub content: ContentChanges,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageChanges {
    pub before: u32,
    pub after: u32,
    pub rotated: u32,
    pub inserted: u32,
    pub deleted: u32,
    pub moved: u32,
}

/// Annotations added, by kind; redaction marks still in the file; base annotations gone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnnotationChanges {
    pub stamps: u32,
    pub drawings: u32,
    pub comments: u32,
    pub redaction_marks: u32,
    pub other: u32,
    pub removed: u32,
}

/// Among pages present in both files: those that lost words, and those that gained some.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextChanges {
    pub pages_with_text_removed: u32,
    pub pages_with_text_added: u32,
}

/// Among pages present in both files: those that lost an image (or part of one: a cleared image is
/// a new one), and those that lost text or an image, each page once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentChanges {
    pub pages_with_images_removed: u32,
    pub pages_with_content_removed: u32,
}

/// Which file could not be read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectError {
    Base(String),
    Edited(String),
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// An image placement: pixel size, box in user space (whole points), and its file's hash.
type ImageKey = (u32, u32, [i64; 4], Option<u64>);

/// Past this many images on a page, placements are compared without hashing each image's file,
/// which decodes it: a cleared image of the same size and box is then not seen, a removed one is.
const MAX_HASHED_IMAGES: usize = 64;

struct Read {
    info: DocInfo,
    /// Each page's words, in reading order.
    pages: Vec<Vec<String>>,
    /// Each page's image placements, counted.
    images: Vec<BTreeMap<ImageKey, u32>>,
}

fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn page_image_keys(document: &crate::Document, page: usize) -> BTreeMap<ImageKey, u32> {
    let images = document.page_images(page);
    let hashed = images.len() <= MAX_HASHED_IMAGES;
    let mut keys = BTreeMap::new();
    for (index, image) in images.iter().enumerate() {
        let file = if hashed { document.page_image_file(page, index).ok().map(|(_, bytes)| hash_of(&bytes)) } else { None };
        let key = (image.width, image.height, image.rect.map(|value| value.round() as i64), file);
        let slot = keys.entry(key).or_insert(0u32);
        *slot = slot.saturating_add(1);
    }
    keys
}

fn read(bytes: &[u8]) -> Result<Read, String> {
    let shared = Arc::new(bytes.to_vec());
    let info = pdfcraft_render::inspect(shared.clone(), None).map_err(|error| format!("{error:?}"))?;
    if info.pages.len() > MAX_PAGES {
        return Err(format!("more than {MAX_PAGES} pages"));
    }
    let mut pages = vec![Vec::new(); info.pages.len()];
    let mut images = vec![BTreeMap::new(); info.pages.len()];
    if !info.encrypted {
        let mut session = Session::new();
        let id = session.open("inspected.pdf", None, shared, None).map_err(|error| format!("{error:?}"))?;
        let document = session.get(id).ok_or_else(|| "the file did not open".to_string())?;
        for word in document.words() {
            if let Some(page) = pages.get_mut(word.page) {
                page.push(word.text);
            }
        }
        for (page, keys) in images.iter_mut().enumerate() {
            *keys = page_image_keys(document, page);
        }
    }
    Ok(Read { info, pages, images })
}

pub fn inspect(base: &[u8], edited: &[u8]) -> Result<Inspection, InspectError> {
    let before = read(base).map_err(InspectError::Base)?;
    let base_kept = edited.starts_with(base);
    let after = match read(edited) {
        Ok(after) if !after.info.encrypted => after,
        Ok(_) => return Ok(Inspection { encrypted: true, base_kept, ..Inspection::default() }),
        // A file that needs a password does not open at all.
        Err(_) if edited.windows(8).any(|window| window == b"/Encrypt") => {
            return Ok(Inspection { encrypted: true, base_kept, ..Inspection::default() });
        }
        Err(message) => return Err(InspectError::Edited(message)),
    };
    let mut inspection = Inspection { base_kept, ..Inspection::default() };
    compare_pages(&before, &after, &mut inspection);
    inspection.form_fields_changed = fields_changed(&before.info, &after.info);
    inspection.annotations = annotation_changes(&before.info.annotations, &after.info.annotations);
    Ok(inspection)
}

fn fingerprint(words: &[String]) -> u64 {
    let mut hasher = DefaultHasher::new();
    words.hash(&mut hasher);
    hasher.finish()
}

/// Longest common subsequence of page fingerprints, as (base page, edited page) pairs.
fn matched_pages(a: &[u64], b: &[u64]) -> Vec<(usize, usize)> {
    let width = b.len().saturating_add(1);
    let mut table = vec![0u16; a.len().saturating_add(1).saturating_mul(width)];
    let cell = |table: &[u16], i: usize, j: usize| table.get(i.saturating_mul(width).saturating_add(j)).copied().unwrap_or(0);
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            let value =
                if a.get(i) == b.get(j) { cell(&table, i + 1, j + 1).saturating_add(1) } else { cell(&table, i + 1, j).max(cell(&table, i, j + 1)) };
            if let Some(slot) = table.get_mut(i.saturating_mul(width).saturating_add(j)) {
                *slot = value;
            }
        }
    }
    let (mut i, mut j, mut pairs) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        if a.get(i) == b.get(j) {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if cell(&table, i + 1, j) >= cell(&table, i, j + 1) {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Whether a placement of the base page is missing from the edited page.
fn images_lost(before: Option<&BTreeMap<ImageKey, u32>>, after: Option<&BTreeMap<ImageKey, u32>>) -> bool {
    let Some(before) = before else { return false };
    before.iter().any(|(key, count)| after.and_then(|after| after.get(key)).copied().unwrap_or(0) < *count)
}

/// Words removed and words added, as a multiset.
fn word_changes(before: &[String], after: &[String]) -> (bool, bool) {
    let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
    for word in before {
        *counts.entry(word).or_default() += 1;
    }
    for word in after {
        *counts.entry(word).or_default() -= 1;
    }
    (counts.values().any(|value| *value > 0), counts.values().any(|value| *value < 0))
}

fn compare_pages(before: &Read, after: &Read, inspection: &mut Inspection) {
    let base_prints: Vec<u64> = before.pages.iter().map(|words| fingerprint(words)).collect();
    let edited_prints: Vec<u64> = after.pages.iter().map(|words| fingerprint(words)).collect();
    let in_order = matched_pages(&base_prints, &edited_prints);
    let used_base: BTreeSet<usize> = in_order.iter().map(|pair| pair.0).collect();
    let used_edited: BTreeSet<usize> = in_order.iter().map(|pair| pair.1).collect();
    let mut base_free: Vec<usize> = (0..base_prints.len()).filter(|page| !used_base.contains(page)).collect();
    let mut edited_free: Vec<usize> = (0..edited_prints.len()).filter(|page| !used_edited.contains(page)).collect();
    let mut same_text = in_order;
    // The same words elsewhere: a moved page.
    let mut moved = 0usize;
    base_free.retain(|&page| {
        let Some(position) = edited_free.iter().position(|&other| base_prints.get(page) == edited_prints.get(other)) else { return true };
        same_text.push((page, edited_free.remove(position)));
        moved += 1;
        false
    });
    // Left at the same places: the same page with changed text (a redaction removes words).
    let changed: Vec<(usize, usize)> = base_free.iter().copied().zip(edited_free.iter().copied()).collect();
    let rotation = |info: &DocInfo, page: usize| info.pages.get(page).map(|page| page.rotation);
    let rotated = same_text.iter().chain(&changed).filter(|(old, new)| rotation(&before.info, *old) != rotation(&after.info, *new)).count();
    let (mut removed, mut added, mut images_removed, mut content_removed) = (0usize, 0usize, 0usize, 0usize);
    // Pages with the same words can still have lost an image; pages left at the same places may
    // have lost or gained words too.
    let pairs = same_text.iter().map(|pair| (*pair, false)).chain(changed.iter().map(|pair| (*pair, true)));
    for ((old, new), text_differs) in pairs {
        let (lost, gained) = if text_differs {
            word_changes(before.pages.get(old).map_or(&[][..], Vec::as_slice), after.pages.get(new).map_or(&[][..], Vec::as_slice))
        } else {
            (false, false)
        };
        let lost_image = images_lost(before.images.get(old), after.images.get(new));
        removed += usize::from(lost);
        added += usize::from(gained);
        images_removed += usize::from(lost_image);
        content_removed += usize::from(lost || lost_image);
    }
    inspection.pages = PageChanges {
        before: count(base_prints.len()),
        after: count(edited_prints.len()),
        rotated: count(rotated),
        inserted: count(edited_free.len().saturating_sub(changed.len())),
        deleted: count(base_free.len().saturating_sub(changed.len())),
        moved: count(moved),
    };
    inspection.text = TextChanges { pages_with_text_removed: count(removed), pages_with_text_added: count(added) };
    inspection.content = ContentChanges { pages_with_images_removed: count(images_removed), pages_with_content_removed: count(content_removed) };
}

fn fields_changed(before: &DocInfo, after: &DocInfo) -> u32 {
    let values = |info: &DocInfo| info.fields.iter().map(|field| (field.name.clone(), field.value.clone())).collect::<BTreeMap<_, _>>();
    let (old, new) = (values(before), values(after));
    let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    count(names.into_iter().filter(|name| old.get(*name) != new.get(*name)).count())
}

type AnnotationKey = (String, [i64; 4], String, String);

fn annotation_key(annotation: &Annotation) -> AnnotationKey {
    (
        annotation.subtype.clone(),
        annotation.rect.map(|value| value.round() as i64),
        annotation.contents.clone().unwrap_or_default(),
        annotation.author.clone().unwrap_or_default(),
    )
}

/// Pop-ups belong to their markup; widgets are form fields; links are not edits here.
fn ignored(subtype: &str) -> bool {
    matches!(subtype, "Popup" | "Widget" | "Link")
}

fn annotation_changes(before: &[Annotation], after: &[Annotation]) -> AnnotationChanges {
    let mut counts: BTreeMap<AnnotationKey, i64> = BTreeMap::new();
    for annotation in before.iter().filter(|annotation| !ignored(&annotation.subtype)) {
        *counts.entry(annotation_key(annotation)).or_default() += 1;
    }
    for annotation in after.iter().filter(|annotation| !ignored(&annotation.subtype)) {
        *counts.entry(annotation_key(annotation)).or_default() -= 1;
    }
    let mut changes = AnnotationChanges {
        redaction_marks: count(after.iter().filter(|annotation| annotation.subtype == "Redact").count()),
        ..AnnotationChanges::default()
    };
    for ((subtype, ..), difference) in counts {
        if difference > 0 {
            changes.removed = changes.removed.saturating_add(u32::try_from(difference).unwrap_or(u32::MAX));
            continue;
        }
        let added = u32::try_from(difference.unsigned_abs()).unwrap_or(u32::MAX);
        let slot = match subtype.as_str() {
            "Stamp" => &mut changes.stamps,
            "Ink" | "Line" | "Square" | "Circle" | "Polygon" | "PolyLine" => &mut changes.drawings,
            "Text" | "FreeText" | "Highlight" | "Underline" | "StrikeOut" | "Squiggly" | "Caret" | "FileAttachment" | "Sound" => {
                &mut changes.comments
            }
            // Counted above, whether added now or kept from the base.
            "Redact" => continue,
            _ => &mut changes.other,
        };
        *slot = slot.saturating_add(added);
    }
    changes
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use super::*;
    use crate::tests::fixture;
    use crate::{Edit, Markup, NewAnnotation, Session, Shape, Style, rect_quad};

    /// A small grey PNG, for an image placed on a page.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&vec![128u8; (width * height) as usize]).unwrap();
        }
        bytes
    }

    fn edited(pages: usize, edits: Vec<Edit>) -> (Vec<u8>, Vec<u8>) {
        let base = fixture(pages);
        let mut session = Session::new().with_clock(|| 1_700_000_000);
        let id = session.open("base.pdf", None, Arc::new(base.clone()), None).unwrap();
        for edit in edits {
            session.apply(id, edit).unwrap();
        }
        let saved = session.save_bytes(id).unwrap();
        (base, saved.as_ref().clone())
    }

    fn annotation(page: usize, shape: Shape) -> Edit {
        Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents: String::new(), author: "Sari".into() })
    }

    #[test]
    fn finds_nothing_in_an_unchanged_file() {
        let base = fixture(2);
        let inspection = inspect(&base, &base).unwrap();
        assert_eq!(
            inspection,
            Inspection { base_kept: true, pages: PageChanges { before: 2, after: 2, ..PageChanges::default() }, ..Inspection::default() }
        );
    }

    #[test]
    fn reports_a_rotation_as_an_appended_update() {
        let (base, saved) = edited(2, vec![Edit::RotatePages { pages: vec![0], degrees: 90 }]);
        let inspection = inspect(&base, &saved).unwrap();
        assert!(inspection.base_kept);
        assert_eq!(inspection.pages, PageChanges { before: 2, after: 2, rotated: 1, ..PageChanges::default() });
    }

    #[test]
    fn reports_deleted_moved_and_inserted_pages() {
        let (base, saved) = edited(3, vec![Edit::DeletePages { pages: vec![2] }]);
        assert_eq!(inspect(&base, &saved).unwrap().pages.deleted, 1);
        let (base, saved) = edited(3, vec![Edit::MovePages { pages: vec![2], to: 0 }]);
        assert_eq!(inspect(&base, &saved).unwrap().pages.moved, 1);
        let (base, saved) = edited(2, vec![Edit::InsertBlankPage { at: 2, width: 200.0, height: 300.0 }]);
        assert_eq!(inspect(&base, &saved).unwrap().pages, PageChanges { before: 2, after: 3, inserted: 1, ..PageChanges::default() });
    }

    #[test]
    fn reports_an_applied_redaction_as_a_rewrite_that_removed_text() {
        let mark = Shape::Redact { quads: vec![rect_quad([80.0, 140.0, 100.0, 180.0])], overlay: String::new(), look: Default::default() };
        let (base, saved) = edited(2, vec![annotation(0, mark), Edit::ApplyRedactions { pages: None }]);
        let inspection = inspect(&base, &saved).unwrap();
        assert!(!inspection.base_kept);
        assert_eq!(inspection.text, TextChanges { pages_with_text_removed: 1, pages_with_text_added: 0 });
        assert_eq!(inspection.content.pages_with_content_removed, 1);
        assert_eq!(inspection.annotations.redaction_marks, 0);
    }

    // A signature or a stamp image is a typical redaction target (D11): a rewrite that removed
    // only an image is a redaction too. The fixture's text sits at y 126–150 of a 200 × 300 page
    // (display space, as `AddImage` and the marks take it); the image sits well below it.
    #[test]
    fn reports_a_redaction_that_removed_only_an_image() {
        let (_, with_image) =
            edited(1, vec![Edit::AddImage { page: 0, rect: Some([20.0, 220.0, 60.0, 260.0]), name: "seal.png".into(), bytes: Arc::new(png(8, 8)) }]);
        let mut session = Session::new().with_clock(|| 1_700_000_000);
        let id = session.open("base.pdf", None, Arc::new(with_image.clone()), None).unwrap();
        assert_eq!(session.get(id).unwrap().page_images(0).len(), 1, "the base shows the image");
        let mark = Shape::Redact { quads: vec![rect_quad([10.0, 210.0, 70.0, 270.0])], overlay: String::new(), look: Default::default() };
        session.apply(id, annotation(0, mark)).unwrap();
        session.apply(id, Edit::ApplyRedactions { pages: None }).unwrap();
        let redacted = session.save_bytes(id).unwrap().as_ref().clone();
        let inspection = inspect(&with_image, &redacted).unwrap();
        assert!(!inspection.base_kept);
        assert_eq!(inspection.text, TextChanges::default());
        assert_eq!(inspection.content, ContentChanges { pages_with_images_removed: 1, pages_with_content_removed: 1 });
    }

    #[test]
    fn counts_a_redaction_mark_saved_without_applying_it() {
        let mark = Shape::Redact { quads: vec![rect_quad([80.0, 140.0, 100.0, 180.0])], overlay: String::new(), look: Default::default() };
        let (base, saved) = edited(2, vec![annotation(0, mark)]);
        let inspection = inspect(&base, &saved).unwrap();
        assert!(inspection.base_kept);
        assert_eq!(inspection.annotations.redaction_marks, 1);
        assert_eq!(inspection.text, TextChanges::default());
    }

    #[test]
    fn counts_drawings_and_comments_added() {
        let ink = Shape::Ink { strokes: vec![vec![[10.0, 10.0], [60.0, 60.0]]] };
        let highlight = Shape::TextMarkup { kind: Markup::Highlight, quads: vec![rect_quad([20.0, 140.0, 100.0, 180.0])] };
        let (base, saved) = edited(1, vec![annotation(0, ink), annotation(0, highlight)]);
        let annotations = inspect(&base, &saved).unwrap().annotations;
        assert_eq!((annotations.drawings, annotations.comments, annotations.removed), (1, 1, 0));
    }

    #[test]
    fn reports_encrypted_output_whether_or_not_it_opens() {
        let base = fixture(1);
        let encrypted = |user: &str| {
            let mut document = pdfcraft_cos::Document::open(Arc::new(fixture(1))).unwrap();
            document
                .set_encryption(&pdfcraft_cos::NewEncryption {
                    algorithm: pdfcraft_cos::Algorithm::Aes256,
                    user_password: user,
                    owner_password: "owner",
                    permissions: -1,
                    encrypt_metadata: true,
                    seed: [7; 32],
                })
                .unwrap();
            pdfcraft_cos::write_full(&document, &Default::default()).unwrap()
        };
        assert!(inspect(&base, &encrypted("")).unwrap().encrypted);
        assert!(inspect(&base, &encrypted("pw")).unwrap().encrypted);
    }

    #[test]
    fn reports_a_form_field_filled_in() {
        let base = crate::tests::form_fixture();
        let mut session = Session::new().with_clock(|| 1_700_000_000);
        let id = session.open("form.pdf", None, Arc::new(base.clone()), None).unwrap();
        session.apply(id, Edit::SetFieldValue { name: "name".into(), value: crate::FieldValue::Text("Ada".into()) }).unwrap();
        let saved = session.save_bytes(id).unwrap().as_ref().clone();
        let inspection = inspect(&base, &saved).unwrap();
        assert_eq!(inspection.form_fields_changed, 1);
        // The field's widget is the form, not an annotation someone added.
        assert_eq!(inspection.annotations, AnnotationChanges::default());
    }

    #[test]
    fn reports_a_rewrite_whose_base_is_not_a_prefix() {
        let base = fixture(2);
        let mut rewritten = fixture(2);
        // Same pages, different bytes up front: a full rewrite, not an appended update.
        rewritten.splice(9..9, b"% rewritten\n".iter().copied());
        let inspection = inspect(&base, &rewritten).unwrap();
        assert!(!inspection.base_kept);
        assert_eq!(inspection.pages, PageChanges { before: 2, after: 2, ..PageChanges::default() });
    }

    #[test]
    fn refuses_more_pages_than_the_bound() {
        let small = fixture(1);
        let large = fixture(MAX_PAGES + 1);
        assert!(matches!(inspect(&large, &small), Err(InspectError::Base(_))));
        assert!(matches!(inspect(&small, &large), Err(InspectError::Edited(_))));
    }

    #[test]
    fn never_panics_on_garbage_or_a_cut_file() {
        let base = fixture(2);
        // A fixed pseudo-random stream, so the case is the same on every run.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect();
        let mut header_and_noise = b"%PDF-1.7\n".to_vec();
        header_and_noise.extend_from_slice(&noise);
        for bad in [Vec::new(), noise.clone(), header_and_noise, b"%PDF-1.7\n".to_vec()] {
            assert!(matches!(inspect(&bad, &base), Err(InspectError::Base(_))));
            assert!(matches!(inspect(&base, &bad), Err(InspectError::Edited(_))));
        }
        // A file cut short may be repaired or refused; either way it is an answer, not a panic.
        for length in [base.len() / 4, base.len() / 2, base.len().saturating_sub(10)] {
            let cut = base.get(..length).unwrap().to_vec();
            let _ = inspect(&base, &cut);
            let _ = inspect(&cut, &base);
        }
    }

    #[test]
    fn names_the_file_it_cannot_read() {
        let base = fixture(1);
        assert!(matches!(inspect(b"not a pdf", &base), Err(InspectError::Base(_))));
        assert!(matches!(inspect(&base, b"PK\x03\x04 not a pdf"), Err(InspectError::Edited(_))));
    }
}
