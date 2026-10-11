//! Redaction and the structure tree: tags must not keep what was removed from the page.
//!
//! Marked content (MCIDs) that applying redactions changed is found by comparing each MCID's
//! operators before and after. Elements owning such content lose their alternate text, actual
//! text and expansion (which may spell out the removed words), and so do their ancestors, whose
//! text stands for their descendants' content too (Embed build, R6b-47); references to marked
//! content that is now empty are dropped from the element.

use std::collections::{HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object};

/// Each MCID's operators (serialized), and whether anything is still drawn inside it. `named`:
/// the MCIDs of the property lists named in the resources.
fn by_mcid(data: &[u8], named: &HashMap<Vec<u8>, i64>) -> (HashMap<i64, (Vec<u8>, bool)>, bool) {
    let mut out: HashMap<i64, (Vec<u8>, bool)> = HashMap::new();
    let mut stack: Vec<Option<i64>> = Vec::new();
    // Embed build: an `EMC` closing nothing, or a level left open (R6b-47).
    let mut unbalanced = false;
    for op in pdfcraft_content::parse(data).ops {
        match op.op.as_slice() {
            b"BDC" => {
                let id = match op.operands.get(1) {
                    Some(Object::Dict(d)) => d.get(b"MCID").and_then(Object::as_int),
                    // Embed build: a list named in the resources has its MCID there (R6b-47).
                    Some(Object::Name(name)) => named.get(name).copied(),
                    _ => None,
                };
                if let Some(id) = id {
                    out.entry(id).or_default();
                }
                stack.push(id);
                continue;
            }
            b"BMC" => {
                stack.push(None);
                continue;
            }
            b"EMC" => {
                unbalanced |= stack.pop().is_none();
                continue;
            }
            _ => {}
        }
        let draws = match op.op.as_slice() {
            b"Tj" | b"'" | b"\"" => op.operands.last().and_then(Object::as_string).is_some_and(|s| !s.bytes.is_empty()),
            b"TJ" => {
                op.operands.first().and_then(Object::as_array).is_some_and(|a| a.iter().any(|x| x.as_string().is_some_and(|s| !s.bytes.is_empty())))
            }
            b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"S" | b"s" | b"sh" | b"Do" | b"BI" => true,
            _ => false,
        };
        let mut bytes = Vec::new();
        pdfcraft_content::write_op(&op, &mut bytes);
        for id in stack.iter().flatten() {
            let e = out.entry(*id).or_default();
            e.0.extend_from_slice(&bytes);
            e.1 |= draws;
        }
    }
    (out, unbalanced || !stack.is_empty())
}

/// The MCIDs whose content changed between `before` and `after`, and those left empty. `named`:
/// the MCIDs of the property lists named in the resources (see [`named_mcids`]).
pub(crate) fn touched(before: &[u8], after: &[u8], named: &HashMap<Vec<u8>, i64>) -> (HashSet<i64>, HashSet<i64>) {
    let ((b, unbalanced), (a, _)) = (by_mcid(before, named), by_mcid(after, named));
    let mut changed = HashSet::new();
    let mut empty = HashSet::new();
    // Embed build: which content an MCID holds is each reader's guess where marked content is
    // unbalanced: every MCID counts as changed (R6b-47).
    if unbalanced {
        changed.extend(b.keys().copied());
    }
    for (id, (ops, _)) in &b {
        match a.get(id) {
            Some((new, draws)) => {
                if new != ops {
                    changed.insert(*id);
                }
                if !draws {
                    empty.insert(*id);
                }
            }
            None => {
                changed.insert(*id);
                empty.insert(*id);
            }
        }
    }
    (changed, empty)
}

/// Embed build: the MCID of each property list named in `resources`' `/Properties` (R6b-47).
pub(crate) fn named_mcids(doc: &Document, resources: &Dict) -> HashMap<Vec<u8>, i64> {
    let properties = resources.get(b"Properties").map(|properties| doc.resolve(properties));
    let Some(properties) = properties.as_deref().and_then(Object::as_dict) else { return HashMap::new() };
    properties
        .iter()
        .filter_map(|(name, list)| {
            doc.resolve(list).as_dict().and_then(|list| list.get(b"MCID")).and_then(Object::as_int).map(|mcid| (name.clone(), mcid))
        })
        .collect()
}

fn mcid_of(o: &Object, page: Option<ObjRef>, target: ObjRef) -> Option<i64> {
    // Embed build: a kid no element names a page for is taken as the page's (a reader may),
    // and a marked-content reference into a form XObject (`/Stm`) is the form's, not the
    // page's (R6b-47).
    match o {
        Object::Int(n) if page.is_none_or(|page| page == target) => Some(*n),
        Object::Dict(m) if m.name(b"Type") == Some(b"MCR") && !m.contains(b"Stm") => {
            let pg = m.get(b"Pg").and_then(Object::as_ref).or(page);
            pg.is_none_or(|pg| pg == target).then(|| m.get(b"MCID").and_then(Object::as_int)).flatten()
        }
        _ => None,
    }
}

/// The structure tree's elements (indirect, with `/S`), each with its dictionary, the page it
/// or its nearest ancestor names, and (Embed build) the element it is in.
struct Element {
    obj: ObjRef,
    dict: Dict,
    page: Option<ObjRef>,
}

fn elements(doc: &Document) -> (Vec<Element>, HashMap<ObjRef, ObjRef>) {
    let mut out = Vec::new();
    let mut parents: HashMap<ObjRef, ObjRef> = HashMap::new();
    let Some(root) = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().cloned())
        .and_then(|c| c.get(b"StructTreeRoot").map(|s| doc.resolve(s)).and_then(|s| s.as_dict().cloned()))
    else {
        return (out, parents);
    };
    let mut todo: Vec<(Object, Option<ObjRef>, Option<ObjRef>)> = root.get(b"K").map(|k| vec![(k.clone(), None, None)]).unwrap_or_default();
    let mut seen = HashSet::new();
    while let Some((k, pg, parent)) = todo.pop() {
        match k {
            Object::Array(a) => todo.extend(a.into_iter().map(|x| (x, pg, parent))),
            Object::Ref(r) => {
                if !seen.insert(r) || seen.len() > 2_000_000 {
                    continue;
                }
                let Some(d) = doc.get(r).as_dict().cloned() else { continue };
                if !d.contains(b"S") {
                    continue;
                }
                if let Some(parent) = parent {
                    parents.insert(r, parent);
                }
                let pg = d.get(b"Pg").and_then(Object::as_ref).or(pg);
                let kids: Vec<Object> = match d.get(b"K") {
                    Some(Object::Array(a)) => a.clone(),
                    Some(o) => vec![o.clone()],
                    None => Vec::new(),
                };
                todo.extend(kids.into_iter().map(|x| (x, pg, Some(r))));
                out.push(Element { obj: r, dict: d, page: pg });
            }
            _ => {}
        }
    }
    (out, parents)
}

/// An element's kids, each resolved (Embed build: a marked-content reference may be an object of
/// its own).
fn kids_of(doc: &Document, element: &Dict) -> Vec<(Object, Object)> {
    let kids: Vec<Object> = match element.get(b"K") {
        Some(Object::Array(a)) => a.clone(),
        Some(o) => vec![o.clone()],
        None => Vec::new(),
    };
    kids.into_iter()
        .map(|kid| {
            let resolved = match &kid {
                Object::Ref(r) => (*doc.get(*r)).clone(),
                other => other.clone(),
            };
            (kid, resolved)
        })
        .collect()
}

const STANDS_FOR_CONTENT: [&[u8]; 3] = [b"Alt", b"ActualText", b"E"];

/// Take the text standing for content off `hits` and (Embed build, R6b-47) their ancestors,
/// whose text stands for their descendants' content too; each once. Returns how many changed.
fn strip_with_ancestors(doc: &mut Document, hits: &[ObjRef], parents: &HashMap<ObjRef, ObjRef>) -> Result<usize, pdfcraft_cos::CosError> {
    let mut all: Vec<ObjRef> = Vec::new();
    let mut done: HashSet<ObjRef> = HashSet::new();
    for hit in hits {
        let mut at = Some(*hit);
        while let Some(element) = at {
            if !done.insert(element) {
                break;
            }
            all.push(element);
            at = parents.get(&element).copied();
        }
    }
    let mut n = 0;
    for r in all {
        let has_text = doc.get(r).as_dict().is_some_and(|e| STANDS_FOR_CONTENT.iter().any(|key| e.contains(key)));
        if has_text {
            doc.update_dict(r, |e| {
                for key in STANDS_FOR_CONTENT {
                    e.remove(key);
                }
            })?;
            n += 1;
        }
    }
    Ok(n)
}

/// Clean the elements owning `changed` marked content on `page`; returns how many changed.
pub(crate) fn clean(doc: &mut Document, page: ObjRef, changed: &HashSet<i64>, empty: &HashSet<i64>) -> Result<usize, pdfcraft_cos::CosError> {
    if changed.is_empty() {
        return Ok(0);
    }
    let (elements, parents) = elements(doc);
    let mut hits: Vec<ObjRef> = Vec::new();
    for element in &elements {
        let kids = kids_of(doc, &element.dict);
        if !kids.iter().any(|(_, kid)| mcid_of(kid, element.page, page).is_some_and(|m| changed.contains(&m))) {
            continue;
        }
        hits.push(element.obj);
        // Marked content that is now empty no longer belongs to the element.
        let gone = |kid: &Object| mcid_of(kid, element.page, page).is_some_and(|m| empty.contains(&m));
        if kids.iter().any(|(_, kid)| gone(kid)) {
            let keep: Vec<Object> = kids.iter().filter(|(_, kid)| !gone(kid)).map(|(written, _)| written.clone()).collect();
            let array = matches!(element.dict.get(b"K"), Some(Object::Array(_)));
            doc.update_dict(element.obj, |e| match (array, keep.is_empty()) {
                (true, _) => e.set(b"K".to_vec(), Object::Array(keep)),
                (false, true) => {
                    e.remove(b"K");
                }
                (false, false) => {}
            })?;
        }
    }
    strip_with_ancestors(doc, &hits, &parents)
}

/// Embed build (R6b-47): the structure tree's marked-content references into forms the
/// redaction of `page` rewrote (`/Stm`, on that page or naming none): their elements lose the
/// text standing for changed content (with their ancestors), a reference to content now empty
/// goes, and the others follow the rewritten form, so the original (unredacted) form is no
/// longer kept by the tree. Returns how many elements changed.
pub(crate) fn clean_forms(doc: &mut Document, page: ObjRef, forms: &[crate::interp::FormRewrite]) -> Result<usize, pdfcraft_cos::CosError> {
    if forms.is_empty() {
        return Ok(0);
    }
    let (elements, parents) = elements(doc);
    let mut hits: Vec<ObjRef> = Vec::new();
    for element in &elements {
        let kids = kids_of(doc, &element.dict);
        let mut keep: Vec<Object> = Vec::with_capacity(kids.len());
        let mut touched = false;
        for (written, kid) in kids {
            let rewrite = match &kid {
                Object::Dict(m) if m.name(b"Type") == Some(b"MCR") => {
                    let pg = m.get(b"Pg").and_then(Object::as_ref).or(element.page);
                    let stm = m.get(b"Stm").and_then(Object::as_ref);
                    forms.iter().find(|form| stm == Some(form.old) && pg.is_none_or(|pg| pg == page)).map(|form| (form, m.clone()))
                }
                _ => None,
            };
            let Some((form, mut reference)) = rewrite else {
                keep.push(written);
                continue;
            };
            touched = true;
            let mcid = reference.get(b"MCID").and_then(Object::as_int);
            if mcid.is_some_and(|m| form.changed.contains(&m)) && !hits.contains(&element.obj) {
                hits.push(element.obj);
            }
            if mcid.is_some_and(|m| form.empty.contains(&m)) {
                continue;
            }
            reference.set(b"Stm".to_vec(), Object::Ref(form.new));
            match written {
                Object::Ref(r) => {
                    doc.set(r, Object::Dict(reference));
                    keep.push(Object::Ref(r));
                }
                _ => keep.push(Object::Dict(reference)),
            }
        }
        if touched {
            let array = matches!(element.dict.get(b"K"), Some(Object::Array(_)));
            doc.update_dict(element.obj, |e| match (array, keep.len()) {
                (true, _) | (false, 2..) => e.set(b"K".to_vec(), Object::Array(keep)),
                (false, 0) => {
                    e.remove(b"K");
                }
                (false, _) => {
                    if let Some(only) = keep.into_iter().next() {
                        e.set(b"K".to_vec(), only);
                    }
                }
            })?;
        }
    }
    strip_with_ancestors(doc, &hits, &parents)
}

/// Embed build (follow-up 7 audit, item 10): the elements naming one of the `removed`
/// annotations through an object reference (`/Type /OBJR`) lose the text standing for it (with
/// their ancestors), and the reference goes: kept, it would keep the removed annotation, and
/// what it said, in the file. Returns how many elements changed.
pub(crate) fn clean_objects(doc: &mut Document, removed: &[ObjRef]) -> Result<usize, pdfcraft_cos::CosError> {
    if removed.is_empty() {
        return Ok(0);
    }
    let (elements, parents) = elements(doc);
    let mut hits: Vec<ObjRef> = Vec::new();
    for element in &elements {
        let kids = kids_of(doc, &element.dict);
        let names_removed = |kid: &Object| match kid {
            Object::Dict(reference) if reference.name(b"Type") == Some(b"OBJR") => {
                reference.get(b"Obj").and_then(Object::as_ref).is_some_and(|target| removed.contains(&target))
            }
            _ => false,
        };
        if !kids.iter().any(|(_, kid)| names_removed(kid)) {
            continue;
        }
        hits.push(element.obj);
        let keep: Vec<Object> = kids.iter().filter(|(_, kid)| !names_removed(kid)).map(|(written, _)| written.clone()).collect();
        let array = matches!(element.dict.get(b"K"), Some(Object::Array(_)));
        doc.update_dict(element.obj, |e| match (array, keep.len()) {
            (true, _) | (false, 2..) => e.set(b"K".to_vec(), Object::Array(keep)),
            (false, 0) => {
                e.remove(b"K");
            }
            (false, _) => {
                if let Some(only) = keep.into_iter().next() {
                    e.set(b"K".to_vec(), only);
                }
            }
        })?;
    }
    strip_with_ancestors(doc, &hits, &parents)
}
