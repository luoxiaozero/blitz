//! Enable the dom to participate in styling by servo
//!
use crate::style_traverser;

use std::{
    borrow::{Borrow, Cow},
    cell::{Cell, RefCell},
    collections::HashMap,
};

use atomic_refcell::{AtomicRef, AtomicRefCell, AtomicRefMut};
use dioxus::{core::exports::bumpalo::Bump, prelude::LazyNodes};
use euclid::{Rect, Scale, Size2D};
use fxhash::FxHashMap;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, RcDom};
use selectors::{
    matching::{ElementSelectorFlags, MatchingContext, VisitedHandlingMode},
    sink::Push,
    OpaqueElement,
};
use servo_url::ServoUrl;
use slab::Slab;
use string_cache::{DefaultAtom, EmptyStaticAtomSet, StaticAtomSet};
use style::{
    animation::DocumentAnimationSet,
    context::{
        QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters,
        SharedStyleContext, StyleContext,
    },
    data::ElementData,
    dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot},
    global_style_data::GLOBAL_STYLE_DATA,
    media_queries::MediaType,
    media_queries::{Device as StyleDevice, MediaList},
    properties::{PropertyDeclarationBlock, PropertyId, StyleBuilder},
    selector_parser::SelectorImpl,
    servo_arc::{Arc, ArcBorrow},
    shared_lock::{Locked, SharedRwLock, StylesheetGuards},
    sharing::StyleSharingCandidate,
    stylesheets::{AllowImportRules, DocumentStyleSheet, Origin, Stylesheet},
    stylist::Stylist,
    thread_state::ThreadState,
    traversal::{DomTraversal, PerLevelTraversalData},
    traversal_flags::TraversalFlags,
    values::{AtomIdent, GenericAtomIdent},
    Atom,
};
use style_traits::dom::ElementState;
use taffy::prelude::{Layout, Style, TaffyTree};
use vello::kurbo;

impl crate::Document {
    pub fn resolve_stylist(&mut self) {
        style::thread_state::enter(ThreadState::LAYOUT);

        let guard = &self.dom.guard;
        let guards = StylesheetGuards {
            author: &guard.read(),
            ua_or_user: &guard.read(),
        };

        // Note that html5ever parses the first node as the document, so we need to unwrap it and get the first child
        // For the sake of this demo, it's always just a single body node, but eventually we will want to construct something like the
        // BoxTree struct that servo uses.
        self.stylist.flush(
            &guards,
            Some(self.dom.root_element()),
            Some(&self.snapshots),
        );

        // Build the style context used by the style traversal
        let context = SharedStyleContext {
            traversal_flags: TraversalFlags::empty(),
            stylist: &self.stylist,
            options: GLOBAL_STYLE_DATA.options.clone(),
            guards,
            visited_styles_enabled: false,
            animations: (&DocumentAnimationSet::default()).clone(),
            current_time_for_animations: 0.0,
            snapshot_map: &self.snapshots,
            registered_speculative_painters: &RegisteredPaintersImpl,
        };

        // components/layout_2020/lib.rs:983
        println!("------Pre-traversing the DOM tree -----");
        let root = self.dom.root_element();

        let token = style_traverser::RecalcStyle::pre_traverse(root, &context);

        // Style the elements, resolving their data
        println!("------ Traversing domtree ------",);
        let traverser = style_traverser::RecalcStyle::new(context);
        style::driver::traverse_dom(&traverser, token, None);

        style::thread_state::exit(ThreadState::LAYOUT);
    }
}

pub struct RealDom {
    pub nodes: Slab<NodeData>,

    /// The parsed html5ever dom
    pub document: RcDom,

    pub guard: SharedRwLock,
}

impl RealDom {
    pub fn from_dioxus(nodes: LazyNodes) -> Self {
        Self::new(dioxus_ssr::render_lazy(nodes))
    }

    pub fn root_node(&self) -> BlitzNode {
        BlitzNode(ref_based_alloc(Entry { id: 0, dom: self }))
    }

    pub fn new(html: String) -> RealDom {
        // parse the html into a slab of node
        let mut nodes = Slab::new();

        // parse the html into a document
        let document = html5ever::parse_document(RcDom::default(), Default::default())
            .from_utf8()
            .read_from(&mut html.as_bytes())
            .unwrap();

        fill_slab_with_handles(&mut nodes, document.document.clone(), 0, None);

        RealDom {
            nodes,
            document,
            guard: SharedRwLock::new(),
        }
    }

    pub fn root_element(&self) -> BlitzNode {
        TDocument::as_node(&self.root_node())
            .first_child()
            .unwrap()
            .as_element()
            .unwrap()
    }
}

// Assign IDs to the RcDom nodes by walking the tree and pushing them into the slab
// We just care that the root is 0, all else can be whatever
// Returns the node that just got inserted
fn fill_slab_with_handles(
    slab: &mut Slab<NodeData>,
    node: Handle,
    child_index: usize,
    parent: Option<usize>,
) -> usize {
    // todo: we want to skip filling comments/scripts/control, etc
    // Dioxus-rsx won't generate this however, so we're fine for now, but elements and text nodes are different

    // Reserve an entry
    let id = {
        let entry = slab.vacant_entry();
        let id = entry.key();
        let style: AtomicRefCell<ElementData> = Default::default();
        entry.insert(NodeData {
            id,
            style,
            child_idx: child_index,
            children: vec![],
            node: node.clone(),
            layout_id: Default::default(),
            // layout: Cell::new(Layout::new()),
            // taffy_style: Default::default(),
            parent,
        });
        id
    };

    println!("generating {} ", id);

    // Now go insert its children. We want their IDs to come back here so we know how to walk them.
    // We'll want some sort of linked list thing too to implement NextSibiling, etc
    // We're going to accumulate the children IDs here and then go back and edit the entry
    // All this dance is to make the borrow checker happy.
    slab[id].children = node
        .children
        .borrow()
        .iter()
        .enumerate()
        .map(|(idx, child)| fill_slab_with_handles(slab, child.clone(), idx, Some(id)))
        .collect();

    id
}

#[derive(Debug)]
pub struct NodeData {
    /// Our parent's ID
    pub parent: Option<usize>,

    /// Our Id
    pub id: usize,

    // Which child are we in our parent?
    pub child_idx: usize,

    // What are our children?
    // Might want to use a linkedlist or something better at precise inserts/delets
    pub children: Vec<usize>,

    // might want to make this weak
    pub node: markup5ever_rcdom::Handle,

    // This little bundle of joy is our layout data from taffy and our style data from stylo
    //
    // todo: layout from new taffy
    pub style: AtomicRefCell<ElementData>,

    pub layout_id: Cell<Option<taffy::prelude::NodeId>>,
    // pub layout: Cell<taffy::layout::Layout>,

    // need to make sure we sync this style and the other style...
    // pub taffy_style: RefCell<taffy::style::Style>,
}

// store_children_to_process
// did_process_child
// pub struct DomData {
//     // ... we can probs just get away with using the html5ever types directly. basically just using the servo dom
//     node: markup5ever_rcdom::Node,
//     local_name: html5ever::LocalName,
//     tag_name: markup5ever_rcdom::TagName,
//     namespace: html5ever::Namespace,
// prefix: DomRefCell<Option<html5ever::Prefix>>,
// attrs: DomRefCell<Vec<Dom<Attr>>>,
// id_attribute: DomRefCell<Option<Atom>>,
// is: DomRefCell<Option<LocalName>>,
// style_attribute: DomRefCell<Option<Arc<Locked<PropertyDeclarationBlock>>>>,
// attr_list: MutNullableDom<NamedNodeMap>,
// class_list: MutNullableDom<DOMTokenList>,
//     state: Cell<ElementState>,
// }

// Like, we do even need separate types for elements/nodes/documents?
#[derive(Debug, Clone, Copy)]
pub struct BlitzNode<'a>(pub &'a Entry<'a>);

impl<'a> BlitzNode<'a> {
    pub fn with(&self, id: usize) -> Self {
        Self(ref_based_alloc(Entry { id, dom: self.dom }))
    }

    pub fn bounds(&self, taffy: &TaffyTree) -> kurbo::Rect {
        let taffy_id = self.data().layout_id.get();
        let layout = taffy.layout(taffy_id.unwrap()).unwrap();

        kurbo::Rect {
            x0: layout.location.x.into(),
            y0: layout.location.y.into(),
            x1: (layout.location.x + layout.size.width).into(),
            y1: (layout.location.y + layout.size.height).into(),
        }
    }
}

impl<'a> std::ops::Deref for BlitzNode<'a> {
    type Target = Entry<'a>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct Entry<'a> {
    pub dom: &'a RealDom,
    pub id: usize,
}

impl std::fmt::Debug for Entry<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry").field("id", &self.id).finish()
    }
}

fn ref_based_alloc(entry: Entry) -> &Entry {
    Box::leak(Box::new(entry))
}

impl<'a> BlitzNode<'a> {
    pub fn data(&self) -> &NodeData {
        &self.0.dom.nodes[self.0.id]
    }

    // Get the nth node in the parents child list
    fn forward(&self, n: usize) -> Option<Self> {
        let node = self.data();

        self.dom.nodes[node.parent?]
            .children
            .get(node.child_idx + n)
            .map(|id| self.with(*id))
    }

    fn backward(&self, n: usize) -> Option<Self> {
        let node = self.data();

        if node.child_idx < n {
            return None;
        }

        self.dom.nodes[node.parent?]
            .children
            .get(node.child_idx - n)
            .map(|id| self.with(*id))
    }

    fn is_element(&self) -> bool {
        matches!(
            self.data().node.data,
            markup5ever_rcdom::NodeData::Element { .. }
        )
    }

    fn is_text_node(&self) -> bool {
        matches!(
            self.data().node.data,
            markup5ever_rcdom::NodeData::Text { .. }
        )
    }
}

impl PartialEq for BlitzNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for BlitzNode<'_> {}

impl<'a> TDocument for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self.clone()
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> QuirksMode {
        QuirksMode::NoQuirks
    }

    fn shared_lock(&self) -> &SharedRwLock {
        &self.dom.guard
    }
}

impl<'a> NodeInfo for BlitzNode<'a> {
    fn is_element(&self) -> bool {
        self.is_element()
    }

    fn is_text_node(&self) -> bool {
        self.is_text_node()
    }
}

impl<'a> TShadowRoot for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self.clone()
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        todo!("Shadow roots not implemented")
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        todo!("Shadow roots not implemented")
    }
}

// components/styleaapper.rs:
impl<'a> TNode for BlitzNode<'a> {
    type ConcreteElement = BlitzNode<'a>;
    type ConcreteDocument = BlitzNode<'a>;
    type ConcreteShadowRoot = BlitzNode<'a>;

    fn parent_node(&self) -> Option<Self> {
        self.data().parent.map(|id| self.with(id))
    }

    fn first_child(&self) -> Option<Self> {
        self.data().children.first().map(|id| self.with(*id))
    }

    fn last_child(&self) -> Option<Self> {
        self.data().children.last().map(|id| self.with(*id))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.backward(1)
    }

    fn next_sibling(&self) -> Option<Self> {
        self.forward(1)
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        self.with(0)
    }

    fn is_in_document(&self) -> bool {
        true
    }

    // I think this is the same as parent_node only in the cases when the direct parent is not a real element, forcing us
    // to travel upwards
    //
    // For the sake of this demo, we're just going to return the parent node ann
    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        self.parent_node().and_then(|node| node.as_element())
    }

    fn opaque(&self) -> OpaqueNode {
        OpaqueNode(self.data().node.as_ref() as *const _ as usize)
    }

    fn debug_id(self) -> usize {
        self.id
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        match self.data().node.data {
            markup5ever_rcdom::NodeData::Element { .. } => Some(self.clone()),
            // markup5ever_rcdom::NodeData::Document { .. } => Some(self.clone()),
            _ => None,
        }
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        panic!();
        if self.id != 0 {
            return None;
        };

        Some(self.clone())
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        todo!("Shadow roots aren't real, yet")
    }
}

impl<'a> selectors::Element for BlitzNode<'a> {
    type Impl = SelectorImpl;

    // use the ptr of the rc as the id
    fn opaque(&self) -> selectors::OpaqueElement {
        OpaqueElement::new(self.data().node.as_ref())
    }

    fn parent_element(&self) -> Option<Self> {
        TElement::traversal_parent(&self)
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    // These methods are implemented naively since we only threaded real nodes and not fake nodes
    // we should try and use `find` instead of this foward/backward stuff since its ugly and slow
    fn prev_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.backward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.forward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn first_element_child(&self) -> Option<Self> {
        let mut children = self.dom_children();

        while let Some(child) = children.next() {
            if child.is_element() {
                return Some(child);
            }
        }

        None
    }

    fn is_html_element_in_html_document(&self) -> bool {
        true
    }

    fn has_local_name(
        &self,
        local_name: &<Self::Impl as selectors::SelectorImpl>::BorrowedLocalName,
    ) -> bool {
        let data = self.data();
        match &data.node.data {
            markup5ever_rcdom::NodeData::Element { name, .. } => &name.local == local_name,
            _ => false,
        }
    }

    fn has_namespace(
        &self,
        ns: &<Self::Impl as selectors::SelectorImpl>::BorrowedNamespaceUrl,
    ) -> bool {
        todo!()
    }

    fn is_same_type(&self, other: &Self) -> bool {
        todo!()
    }

    fn attr_matches(
        &self,
        ns: &selectors::attr::NamespaceConstraint<
            &<Self::Impl as selectors::SelectorImpl>::NamespaceUrl,
        >,
        local_name: &<Self::Impl as selectors::SelectorImpl>::LocalName,
        operation: &selectors::attr::AttrSelectorOperation<
            &<Self::Impl as selectors::SelectorImpl>::AttrValue,
        >,
    ) -> bool {
        todo!()
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &<Self::Impl as selectors::SelectorImpl>::NonTSPseudoClass,
        context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        false
    }

    fn match_pseudo_element(
        &self,
        pe: &<Self::Impl as selectors::SelectorImpl>::PseudoElement,
        context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        // todo!()
    }

    fn is_link(&self) -> bool {
        false
        // self.me()
        //     .parsed.data;
        // .borrow()
        // .iter()
        // .any(|(k, _)| k.local == "href")
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn has_id(
        &self,
        id: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        let mut has_id = false;
        self.each_attr_name(|f| {
            if f.as_ref() == "id" {
                has_id = true;
            }
        });
        has_id
    }

    fn has_class(
        &self,
        search_name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        let Some(al) = self.as_element() else {
            return false;
        };
        let data = al.data().node.data.borrow();
        let markup5ever_rcdom::NodeData::Element { name, attrs, .. } = data else {
            return false;
        };
        let attrs = attrs.borrow();

        for attr in attrs.iter() {
            // make sure we only select class attributes
            if attr.name.local.as_ref() != "class" {
                continue;
            }

            // split the class attribute
            for pheme in attr.value.split_ascii_whitespace() {
                if pheme == search_name.as_ref() {
                    return true;
                }
            }
        }

        false
    }

    fn imported_part(
        &self,
        name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> Option<<Self::Impl as selectors::SelectorImpl>::Identifier> {
        None
    }

    fn is_part(&self, name: &<Self::Impl as selectors::SelectorImpl>::Identifier) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.dom_children().next().is_none()
    }

    fn is_root(&self) -> bool {
        self.parent_node().is_none()
    }
}

impl std::hash::Hash for BlitzNode<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_usize(self.id)
    }
}

impl<'a> TElement for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    type TraversalChildrenIterator = Traverser<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self.clone()
    }

    fn traversal_children(&self) -> style::dom::LayoutIterator<Self::TraversalChildrenIterator> {
        LayoutIterator(Traverser {
            dom: self.dom,
            parent: self.clone(),
            child_index: 0,
        })
    }

    fn is_html_element(&self) -> bool {
        self.is_element()
    }

    // not implemented.....
    fn is_mathml_element(&self) -> bool {
        false
    }

    // need to check the namespace, maybe?
    fn is_svg_element(&self) -> bool {
        false
    }

    fn style_attribute(&self) -> Option<ArcBorrow<Locked<PropertyDeclarationBlock>>> {
        // hmmmm, we need to parse the style attribute, maybe?
        None
    }

    fn animation_rule(
        &self,
        _: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn transition_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn state(&self) -> ElementState {
        // todo: we should track this
        ElementState::empty()
    }

    fn has_part_attr(&self) -> bool {
        false
    }

    fn exports_any_part(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&style::Atom> {
        // None
        let data = self.data();
        let attrs = match data.node.data {
            markup5ever_rcdom::NodeData::Element { ref attrs, .. } => attrs,
            _ => return None,
        };

        let attrs = attrs.borrow();

        let attr_id = attrs.iter().find(|id| id.name.local.as_ref() == "id")?;

        let id = attr_id.value.as_ref();
        let atom = Atom::from(id);
        let leadcked = &*Box::leak(Box::new(atom));

        Some(leadcked)
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&style::values::AtomIdent),
    {
        let Some(al) = self.as_element() else {
            return;
        };
        let data = al.data().node.data.borrow();
        let markup5ever_rcdom::NodeData::Element { name, attrs, .. } = data else {
            return;
        };
        let attrs = attrs.borrow();

        for attr in attrs.iter() {
            // make sure we only select class attributes
            if attr.name.local.as_ref() != "class" {
                continue;
            }

            // split the class attribute
            for pheme in attr.value.split_ascii_whitespace() {
                let atom = Atom::from(pheme); // interns the string
                callback(AtomIdent::cast(&atom));
            }
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        let Some(al) = self.as_element() else {
            return;
        };
        let data = al.data().node.data.borrow();
        let markup5ever_rcdom::NodeData::Element { name, attrs, .. } = data else {
            return;
        };
        let attrs = attrs.borrow();

        for attr in attrs.iter() {
            let b = GenericAtomIdent(attr.name.local.clone());
            callback(&b);
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        false
    }

    fn has_snapshot(&self) -> bool {
        // todo: We want to implement snapshots at some point
        false
    }

    fn handled_snapshot(&self) -> bool {
        todo!()
    }

    unsafe fn set_handled_snapshot(&self) {
        todo!()
    }

    unsafe fn set_dirty_descendants(&self) {
        println!("setting dirty descendants");
    }

    unsafe fn unset_dirty_descendants(&self) {
        println!("unsetting dirty descendants");
    }

    fn store_children_to_process(&self, n: isize) {
        todo!()
    }

    fn did_process_child(&self) -> isize {
        todo!()
    }

    unsafe fn ensure_data(&self) -> AtomicRefMut<style::data::ElementData> {
        self.data().style.borrow_mut()
    }

    unsafe fn clear_data(&self) {
        todo!()
    }

    fn has_data(&self) -> bool {
        todo!()
        // true // all nodes should have data
    }

    fn borrow_data(&self) -> Option<AtomicRef<style::data::ElementData>> {
        self.data().style.try_borrow().ok()
    }

    fn mutate_data(&self) -> Option<AtomicRefMut<style::data::ElementData>> {
        self.data().style.try_borrow_mut().ok()
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        false
    }

    fn has_animations(&self, context: &SharedStyleContext) -> bool {
        false
    }

    fn has_css_animations(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<style::selector_parser::PseudoElement>,
    ) -> bool {
        false
    }

    fn has_css_transitions(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<style::selector_parser::PseudoElement>,
    ) -> bool {
        false
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn lang_attr(&self) -> Option<style::selector_parser::AttrValue> {
        None
    }

    fn match_element_lang(
        &self,
        override_lang: Option<Option<style::selector_parser::AttrValue>>,
        value: &style::selector_parser::Lang,
    ) -> bool {
        false
    }

    fn is_html_document_body_element(&self) -> bool {
        self.0.id == 0
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        visited_handling: VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: Push<style::applicable_declarations::ApplicableDeclarationBlock>,
    {
    }

    fn local_name(
        &self,
    ) -> &<style::selector_parser::SelectorImpl as selectors::parser::SelectorImpl>::BorrowedLocalName
    {
        let data = self.data();
        match &data.node.data {
            markup5ever_rcdom::NodeData::Element { name, .. } => &name.local,
            g => panic!("Not an element {g:?}"),
        }
    }

    fn namespace(&self)
    -> &<style::selector_parser::SelectorImpl as selectors::parser::SelectorImpl>::BorrowedNamespaceUrl{
        let data = self.data();
        match &data.node.data {
            markup5ever_rcdom::NodeData::Element { name, .. } => &name.ns,
            _ => panic!("Not an element"),
        }
    }

    fn query_container_size(
        &self,
        display: &style::values::specified::Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        todo!()
    }
}

pub struct Traverser<'a> {
    dom: &'a RealDom,
    parent: BlitzNode<'a>,
    child_index: usize,
}

impl<'a> Iterator for Traverser<'a> {
    type Item = BlitzNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.parent.data().children.get(self.child_index)?;

        let node = self.parent.with(*node);

        self.child_index += 1;

        Some(node)
    }
}

/// Handle custom painters like images for layouting
///
/// todo: actually implement this
pub struct RegisteredPaintersImpl;
impl RegisteredSpeculativePainters for RegisteredPaintersImpl {
    fn get(&self, name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

#[test]
fn assert_size_of_equals() {
    use std::mem;

    // fn assert_layout<E>() {
    //     assert_eq!(
    //         mem::size_of::<SharingCache<E>>(),
    //         mem::size_of::<TypelessSharingCache>()
    //     );
    //     assert_eq!(
    //         mem::align_of::<SharingCache<E>>(),
    //         mem::align_of::<TypelessSharingCache>()
    //     );
    // }

    // let size = mem::size_of::<StyleSharingCandidate<BlitzNode>>();
    // dbg!(size);
}
