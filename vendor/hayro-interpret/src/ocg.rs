use hayro_syntax::object::dict::keys::{BASE_STATE, D, OCGS, OCMD, OCPROPERTIES, OFF, ON, P, TYPE};
use hayro_syntax::object::{Array, Dict, Name, ObjectIdentifier};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

/// Embed build: an optional-content construct that content is drawn inside, as hayro met it
/// while drawing every layer (`InterpreterSettings::all_layers_visible`): what hayro took as
/// optional content (a `BDC` whose property list resolved, an XObject's `/OC`), and every `BDC`
/// tagged `/OC` whatever hayro made of it. For a caller that evaluates each against the
/// document's `/OCProperties` itself and refuses what it cannot (readers differ on these).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LayerMark {
    /// The marked-content tag (`BDC`'s first operand); `None` for an XObject's `/OC`.
    pub tag: Option<Vec<u8>>,
    /// The object hayro took for the optional content (number, generation): a property list
    /// named in `/Properties`, the `/OC` entry of a property list written inline, an XObject's
    /// `/OC`. `None`: written directly, not a dictionary, or no property list at all.
    pub object: Option<(i32, i32)>,
    /// `object` is the `/OC` entry of a property list written inline in the content.
    pub inline: bool,
}

/// Embed build: the marked content open while every layer is drawn, per thread (nested
/// interpretations — patterns, soft masks, Type 3 glyphs — add to what encloses them).
#[derive(Default)]
struct Layers {
    /// One level per open `BDC`/`BMC`/`/OC`, with its mark where it has one.
    stack: Vec<Option<LayerMark>>,
    /// How many levels have a mark.
    open: usize,
    /// The marks open now, outermost first, once asked for.
    snapshot: Option<Arc<[LayerMark]>>,
    /// Every mark met since the interpretation began.
    seen: HashSet<LayerMark>,
    /// An `EMC` closed a level its content stream did not open, or a content stream left one
    /// open at its end.
    unbalanced: bool,
}

std::thread_local! {
    static LAYERS: RefCell<Layers> = RefCell::new(Layers::default());
}

/// Embed build: start an interpretation with no marked content open (`Context::new`).
pub(crate) fn reset_layers() {
    LAYERS.with(|layers| *layers.borrow_mut() = Layers::default());
}

/// Embed build: the marks the content being drawn now is inside, outermost first (`None`: none),
/// while every layer is drawn. The same `Arc` until a mark opens or closes.
pub fn current_layers() -> Option<Arc<[LayerMark]>> {
    LAYERS.with(|layers| {
        let mut layers = layers.borrow_mut();
        if layers.open == 0 {
            return None;
        }
        if layers.snapshot.is_none() {
            layers.snapshot = Some(layers.stack.iter().flatten().cloned().collect());
        }
        layers.snapshot.clone()
    })
}

/// Embed build: since the interpretation began (`Context::new`), every distinct mark met, and
/// whether marked content was unbalanced in a content stream (see [`current_layers`]).
pub fn layers_report() -> (Vec<LayerMark>, bool) {
    LAYERS.with(|layers| {
        let layers = layers.borrow();
        (layers.seen.iter().cloned().collect(), layers.unbalanced)
    })
}

fn push_layer(mark: Option<LayerMark>) {
    LAYERS.with(|layers| {
        let mut layers = layers.borrow_mut();
        if let Some(mark) = &mark {
            layers.open += 1;
            layers.snapshot = None;
            if !layers.seen.contains(mark) {
                layers.seen.insert(mark.clone());
            }
        }
        layers.stack.push(mark);
    });
}

fn pop_layer() {
    LAYERS.with(|layers| {
        let mut layers = layers.borrow_mut();
        if let Some(Some(_)) = layers.stack.pop() {
            layers.open = layers.open.saturating_sub(1);
            layers.snapshot = None;
        }
    });
}

fn mark_unbalanced() {
    LAYERS.with(|layers| layers.borrow_mut().unbalanced = true);
}

pub(crate) struct OcgState {
    inactive_ocgs: HashSet<ObjectIdentifier>,
    visibility_stack: Vec<bool>,
    /// Embed build: every layer shows, whatever the configuration or marked content says (see
    /// `InterpreterSettings::all_layers_visible`), but for the groups and membership
    /// dictionaries in `hidden` (`InterpreterSettings::hidden_layers`); every level opened is
    /// recorded (see [`current_layers`]).
    pub(crate) all_visible: bool,
    hidden: HashSet<ObjectIdentifier>,
    /// Embed build: the levels the content stream being read opened above (a form's content
    /// may close only what it opened).
    floor: usize,
}

impl Drop for OcgState {
    fn drop(&mut self) {
        // Embed build: what the content left open is unbalanced, and closed here.
        if self.all_visible {
            if !self.visibility_stack.is_empty() {
                mark_unbalanced();
            }
            for _ in self.visibility_stack.drain(..) {
                pop_layer();
            }
        }
    }
}

impl OcgState {
    fn dummy() -> Self {
        Self {
            inactive_ocgs: HashSet::default(),
            visibility_stack: vec![],
            all_visible: false,
            hidden: HashSet::default(),
            floor: 0,
        }
    }

    /// Embed build: draw every layer but `hidden` ((number, generation) of groups and
    /// membership dictionaries), and record what is opened (see `all_visible`).
    pub(crate) fn draw_every_layer(&mut self, hidden: &[(i32, i32)]) {
        self.all_visible = true;
        self.hidden = hidden.iter().map(|(num, generation)| ObjectIdentifier::new(*num, *generation)).collect();
    }

    /// Embed build: open a level, visible when its enclosing level is and `active` (the
    /// document's configuration says it shows) or, drawing every layer, when `id` is not hidden.
    fn push(&mut self, active: bool, id: Option<ObjectIdentifier>, mark: Option<LayerMark>) {
        let parent = self.is_visible();
        let visible = if self.all_visible { parent && !id.is_some_and(|id| self.hidden.contains(&id)) } else { parent && active };
        self.visibility_stack.push(visible);
        if self.all_visible {
            push_layer(mark);
        }
    }

    /// Embed build: before a form's content: what it may close (returned, for [`OcgState::leave`]).
    pub(crate) fn enter(&mut self) -> (usize, usize) {
        let saved = (self.floor, self.visibility_stack.len());
        self.floor = self.visibility_stack.len();
        saved
    }

    /// Embed build: after a form's content, which must have closed what it opened.
    pub(crate) fn leave(&mut self, (floor, depth): (usize, usize)) {
        if self.all_visible && self.visibility_stack.len() != depth {
            mark_unbalanced();
        }
        self.floor = floor;
    }

    pub(crate) fn from_catalog(catalog: &Dict<'_>) -> Self {
        let Some(oc_properties) = catalog.get::<Dict<'_>>(OCPROPERTIES) else {
            return Self::dummy();
        };

        let Some(config) = oc_properties.get::<Dict<'_>>(D) else {
            return Self::dummy();
        };

        let mut inactive = HashSet::new();

        let base_state = config
            .get::<Name<'_>>(BASE_STATE)
            .and_then(|b| BaseState::from_name(b.as_ref()));

        if base_state.unwrap_or(BaseState::On) == BaseState::Off
            && let Some(ocgs) = oc_properties.get::<Array<'_>>(OCGS)
        {
            for item in ocgs.raw_iter() {
                if let Some(ref_) = item.as_obj_ref() {
                    let id: ObjectIdentifier = ref_.into();
                    inactive.insert(id);
                }
            }
        }

        let mut read_ocg_array = |key, insert_active: bool| {
            if let Some(arr) = config.get::<Array<'_>>(key) {
                for item in arr.raw_iter() {
                    if let Some(ref_) = item.as_obj_ref() {
                        let id: ObjectIdentifier = ref_.into();
                        if insert_active {
                            inactive.remove(&id);
                        } else {
                            inactive.insert(id);
                        }
                    }
                }
            }
        };

        read_ocg_array(ON, true);
        read_ocg_array(OFF, false);

        Self {
            inactive_ocgs: inactive,
            visibility_stack: Vec::new(),
            all_visible: false,
            hidden: HashSet::default(),
            floor: 0,
        }
    }

    pub(crate) fn begin_single_oc(&mut self, ocg_id: ObjectIdentifier, mark: Option<LayerMark>) {
        let is_active = !self.inactive_ocgs.contains(&ocg_id);
        self.push(is_active, Some(ocg_id), mark);
    }

    pub(crate) fn begin_ocmd(&mut self, ocmd: &Dict<'_>, id: Option<ObjectIdentifier>, mark: Option<LayerMark>) {
        let policy = ocmd
            .get::<Name<'_>>(P)
            .and_then(|n| OcmdPolicy::from_name(n.as_ref()))
            .unwrap_or(OcmdPolicy::AnyOn);

        let mut ocg_ids: Vec<ObjectIdentifier> = Vec::new();

        if let Some(arr) = ocmd.get::<Array<'_>>(OCGS) {
            for item in arr.raw_iter() {
                if let Some(ref_) = item.as_obj_ref() {
                    ocg_ids.push(ref_.into());
                }
            }
        } else if let Some(ref_) = ocmd.get_ref(OCGS) {
            ocg_ids.push(ref_.into());
        }

        let is_active = if ocg_ids.is_empty() {
            true
        } else {
            match policy {
                OcmdPolicy::AllOn => ocg_ids.iter().all(|id| !self.inactive_ocgs.contains(id)),
                OcmdPolicy::AnyOn => ocg_ids.iter().any(|id| !self.inactive_ocgs.contains(id)),
                OcmdPolicy::AnyOff => ocg_ids.iter().any(|id| self.inactive_ocgs.contains(id)),
                OcmdPolicy::AllOff => ocg_ids.iter().all(|id| self.inactive_ocgs.contains(id)),
            }
        };

        self.push(is_active, id, mark);
    }

    pub(crate) fn begin_ocg(&mut self, props: &Dict<'_>, ref_id: ObjectIdentifier, mark: Option<LayerMark>) {
        match props.get::<Name<'_>>(TYPE).as_deref() {
            Some(OCMD) => self.begin_ocmd(props, Some(ref_id), mark),
            _ => self.begin_single_oc(ref_id, mark),
        }
    }

    pub(crate) fn begin_marked_content(&mut self, mark: Option<LayerMark>) {
        self.push(true, None, mark);
    }

    pub(crate) fn end_marked_content(&mut self) {
        // Embed build: an `EMC` closing what its content stream did not open is unbalanced.
        if self.all_visible && self.visibility_stack.len() <= self.floor {
            mark_unbalanced();
        }
        if self.visibility_stack.pop().is_some() && self.all_visible {
            pop_layer();
        }
    }

    /// PdfCraft patch: override one group's state (viewer layer toggles).
    pub(crate) fn set_active(&mut self, id: ObjectIdentifier, active: bool) {
        if active {
            self.inactive_ocgs.remove(&id);
        } else {
            self.inactive_ocgs.insert(id);
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        // Embed build: drawing every layer, a level hides only what is in a hidden group (see
        // `push`).
        self.visibility_stack.last().copied().unwrap_or(true)
    }
}

impl Default for OcgState {
    fn default() -> Self {
        Self::dummy()
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum BaseState {
    On,
    Off,
    Unchanged,
}

impl BaseState {
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"ON" => Some(Self::On),
            b"OFF" => Some(Self::Off),
            b"Unchanged" => Some(Self::Unchanged),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum OcmdPolicy {
    AllOn,
    AnyOn,
    AnyOff,
    AllOff,
}

impl OcmdPolicy {
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"AllOn" => Some(Self::AllOn),
            b"AnyOn" => Some(Self::AnyOn),
            b"AnyOff" => Some(Self::AnyOff),
            b"AllOff" => Some(Self::AllOff),
            _ => None,
        }
    }
}
