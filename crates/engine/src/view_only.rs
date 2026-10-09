//! View-only sessions (the embed build's view mode): every change to a document is refused here,
//! in the engine, whatever a frontend asks for.

use crate::Session;

/// Why a change was refused: `EditError::ReadOnly` reads "this document can't be edited: …".
pub(crate) const VIEW_ONLY: &str = "it is open for viewing only";

impl Session {
    /// Open documents for viewing only: edits, undo and redo, saves, signing, layer and comment
    /// visibility, and new documents are refused. Applies to documents already open too.
    pub fn set_view_only(&mut self, on: bool) {
        self.view_only = on;
        for doc in &mut self.docs {
            doc.view_only = on;
        }
    }

    pub fn view_only(&self) -> bool {
        self.view_only
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pdfcraft_render::LayerOp;

    use crate::{Edit, EditError, NewAnnotation, Session, Shape, Style};

    /// One page and one optional content group (layer "L1", on).
    fn layered_page() -> Arc<Vec<u8>> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] /D << >> >> >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
            "<< /Type /OCG /Name (L1) >>",
        ];
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            pdf.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        Arc::new(pdf)
    }

    fn square() -> Edit {
        Edit::AddAnnotation(NewAnnotation {
            page: 0,
            shape: Shape::Rectangle { rect: [20.0, 20.0, 80.0, 80.0] },
            style: Style { color: [1.0, 0.0, 0.0], opacity: 1.0, width: 2.0, fill: None },
            contents: "Look here".into(),
            author: "Reviewer".into(),
        })
    }

    #[test]
    fn a_view_only_session_refuses_every_change_and_keeps_the_file() {
        let mut s = Session::new();
        s.set_view_only(true);
        let original = layered_page();
        let id = s.open("view.pdf", None, original.clone(), None).expect("opening is not a change");
        let doc = s.get(id).expect("open");
        assert!(!doc.editable() && !doc.allows_annotation() && !doc.allows_form_filling());
        assert!(!doc.allows_modification() && !doc.allows_assembly());
        assert!(matches!(s.apply(id, square()), Err(EditError::ReadOnly(m)) if m.contains("viewing only")));
        assert!(s.undo(id).is_err());
        assert!(s.redo(id).is_err());
        assert!(s.revert(id).is_err());
        assert!(s.mark_saved(id, original.clone(), None).is_err());
        assert!(s.mark_signed(id, original.clone(), None).is_err());
        assert!(!s.set_hide_comments(id, true));
        assert!(!s.set_layer_visible(id, 0, false));
        assert!(!s.set_layer_state(id, &[(LayerOp::Off, (4, 0))], false));
        assert!(s.open_new("new.pdf", original.clone()).is_err());
        assert!(s.open_revision(id, 1).is_err());
        let doc = s.get(id).expect("still open");
        assert!(!doc.dirty);
        assert!(Arc::ptr_eq(&doc.bytes, &original), "the working file is the file as opened");
        assert!(doc.info.layers.iter().all(|l| l.visible));
    }

    #[test]
    fn the_same_calls_work_without_it_and_it_reaches_documents_already_open() {
        let mut s = Session::new();
        let id = s.open("a.pdf", None, layered_page(), None).expect("opens");
        assert!(s.set_layer_visible(id, 0, false), "the fixture has a layer to hide");
        assert!(s.set_hide_comments(id, true));
        s.set_view_only(true);
        assert!(s.view_only());
        assert!(!s.get(id).expect("open").editable());
        assert!(s.apply(id, square()).is_err());
        s.set_view_only(false);
        assert!(s.apply(id, square()).is_ok());
    }
}
