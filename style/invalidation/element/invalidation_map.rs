/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Code for invalidations due to state or attribute changes.

use crate::context::QuirksMode;
use crate::selector_map::{
    MaybeCaseInsensitiveHashMap, PrecomputedHashMap, SelectorMap, SelectorMapEntry,
};
use crate::selector_parser::{NonTSPseudoClass, SelectorImpl};
use crate::AllocErr;
use crate::{Atom, LocalName, Namespace, ShrinkIfNeeded};
use dom::{DocumentState, ElementState};
use selectors::attr::NamespaceConstraint;
use selectors::parser::{
    Combinator, Component, RelativeSelector, RelativeSelectorCombinatorCount,
    RelativeSelectorMatchHint,
};
use selectors::parser::{Selector, SelectorIter};
use selectors::visitor::{SelectorListKind, SelectorVisitor};
use servo_arc::Arc;
use smallvec::SmallVec;

/// Mapping between (partial) CompoundSelectors (and the combinator to their
/// right) and the states and attributes they depend on.
///
/// In general, for all selectors in all applicable stylesheets of the form:
///
/// |a _ b _ c _ d _ e|
///
/// Where:
///   * |b| and |d| are simple selectors that depend on state (like :hover) or
///     attributes (like [attr...], .foo, or #foo).
///   * |a|, |c|, and |e| are arbitrary simple selectors that do not depend on
///     state or attributes.
///
/// We generate a Dependency for both |a _ b:X _| and |a _ b:X _ c _ d:Y _|,
/// even though those selectors may not appear on their own in any stylesheet.
/// This allows us to quickly scan through the dependency sites of all style
/// rules and determine the maximum effect that a given state or attribute
/// change may have on the style of elements in the document.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct Dependency {
    /// The dependency selector.
    #[ignore_malloc_size_of = "CssRules have primary refs, we measure there"]
    pub selector: Selector<SelectorImpl>,

    /// The offset into the selector that we should match on.
    pub selector_offset: usize,

    /// The parent dependency for an ancestor selector. For example, consider
    /// the following:
    ///
    ///     .foo .bar:where(.baz span) .qux
    ///         ^               ^     ^
    ///         A               B     C
    ///
    ///  We'd generate:
    ///
    ///    * One dependency for .qux (offset: 0, parent: None)
    ///    * One dependency for .baz pointing to B with parent being a
    ///      dependency pointing to C.
    ///    * One dependency from .bar pointing to C (parent: None)
    ///    * One dependency from .foo pointing to A (parent: None)
    ///
    #[ignore_malloc_size_of = "Arc"]
    pub parent: Option<Arc<Dependency>>,

    /// What kind of relative selector invalidation this generates.
    /// None if this dependency is not within a relative selector.
    relative_kind: Option<RelativeDependencyInvalidationKind>,
}

impl SelectorMapEntry for Dependency {
    fn selector(&self) -> SelectorIter<SelectorImpl> {
        self.selector.iter_from(self.selector_offset)
    }
}

/// The kind of elements down the tree this dependency may affect.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, MallocSizeOf)]
pub enum NormalDependencyInvalidationKind {
    /// This dependency may affect the element that changed itself.
    Element,
    /// This dependency affects the style of the element itself, and also the
    /// style of its descendants.
    ///
    /// TODO(emilio): Each time this feels more of a hack for eager pseudos...
    ElementAndDescendants,
    /// This dependency may affect descendants down the tree.
    Descendants,
    /// This dependency may affect siblings to the right of the element that
    /// changed.
    Siblings,
    /// This dependency may affect slotted elements of the element that changed.
    SlottedElements,
    /// This dependency may affect parts of the element that changed.
    Parts,
}

/// The kind of elements up the tree this relative selector dependency may
/// affect. Because this travels upwards, it's not viable for parallel subtree
/// traversal, and is handled separately.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, MallocSizeOf)]
pub enum RelativeDependencyInvalidationKind {
    /// This dependency may affect relative selector anchors for ancestors.
    Ancestors,
    /// This dependency may affect a relative selector anchor for the parent.
    Parent,
    /// This dependency may affect a relative selector anchor for the previous sibling.
    PrevSibling,
    /// This dependency may affect relative selector anchors for ancestors' previous siblings.
    AncestorPrevSibling,
    /// This dependency may affect relative selector anchors for earlier siblings.
    EarlierSibling,
    /// This dependency may affect relative selector anchors for ancestors' earlier siblings.
    AncestorEarlierSibling,
}

/// Invalidation kind merging normal and relative dependencies.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, MallocSizeOf)]
pub enum DependencyInvalidationKind {
    /// This dependency is a normal dependency.
    Normal(NormalDependencyInvalidationKind),
    /// This dependency is a relative dependency.
    Relative(RelativeDependencyInvalidationKind),
}

impl Dependency {
    /// Creates a dummy dependency to invalidate the whole selector.
    ///
    /// This is necessary because document state invalidation wants to
    /// invalidate all elements in the document.
    ///
    /// The offset is such as that Invalidation::new(self) returns a zero
    /// offset. That is, it points to a virtual "combinator" outside of the
    /// selector, so calling combinator() on such a dependency will panic.
    pub fn for_full_selector_invalidation(selector: Selector<SelectorImpl>) -> Self {
        Self {
            selector_offset: selector.len() + 1,
            selector,
            parent: None,
            relative_kind: None,
        }
    }

    /// Returns the combinator to the right of the partial selector this
    /// dependency represents.
    ///
    /// TODO(emilio): Consider storing inline if it helps cache locality?
    fn combinator(&self) -> Option<Combinator> {
        if self.selector_offset == 0 {
            return None;
        }

        Some(
            self.selector
                .combinator_at_match_order(self.selector_offset - 1),
        )
    }

    /// The kind of normal invalidation that this would generate. The dependency
    /// in question must be a normal dependency.
    pub fn normal_invalidation_kind(&self) -> NormalDependencyInvalidationKind {
        debug_assert!(self.relative_kind.is_none(), "Querying normal invalidation kind on relative dependency.");
        match self.combinator() {
            None => NormalDependencyInvalidationKind::Element,
            Some(Combinator::Child) | Some(Combinator::Descendant) => {
                NormalDependencyInvalidationKind::Descendants
            },
            Some(Combinator::LaterSibling) | Some(Combinator::NextSibling) => {
                NormalDependencyInvalidationKind::Siblings
            },
            // TODO(emilio): We could look at the selector itself to see if it's
            // an eager pseudo, and return only Descendants here if not.
            Some(Combinator::PseudoElement) => NormalDependencyInvalidationKind::ElementAndDescendants,
            Some(Combinator::SlotAssignment) => NormalDependencyInvalidationKind::SlottedElements,
            Some(Combinator::Part) => NormalDependencyInvalidationKind::Parts,
        }
    }

    /// The kind of invalidation that this would generate.
    pub fn invalidation_kind(&self) -> DependencyInvalidationKind {
        if let Some(kind) = self.relative_kind {
            return DependencyInvalidationKind::Relative(kind);
        }
        DependencyInvalidationKind::Normal(self.normal_invalidation_kind())
    }
}

/// The same, but for state selectors, which can track more exactly what state
/// do they track.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct StateDependency {
    /// The other dependency fields.
    pub dep: Dependency,
    /// The state this dependency is affected by.
    pub state: ElementState,
}

impl SelectorMapEntry for StateDependency {
    fn selector(&self) -> SelectorIter<SelectorImpl> {
        self.dep.selector()
    }
}

/// The same, but for document state selectors.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct DocumentStateDependency {
    /// We track `Dependency` even though we don't need to track an offset,
    /// since when it changes it changes for the whole document anyway.
    #[cfg_attr(
        feature = "gecko",
        ignore_malloc_size_of = "CssRules have primary refs, we measure there"
    )]
    #[cfg_attr(feature = "servo", ignore_malloc_size_of = "Arc")]
    pub dependency: Dependency,
    /// The state this dependency is affected by.
    pub state: DocumentState,
}

/// Dependency mapping for classes or IDs.
pub type IdOrClassDependencyMap = MaybeCaseInsensitiveHashMap<Atom, SmallVec<[Dependency; 1]>>;
/// Dependency mapping for pseudo-class states.
pub type StateDependencyMap = SelectorMap<StateDependency>;
/// Dependency mapping for attributes.
pub type AttributeDependencyMap = PrecomputedHashMap<LocalName, SmallVec<[Dependency; 1]>>;

/// A map where we store invalidations.
///
/// This is slightly different to a SelectorMap, in the sense of that the same
/// selector may appear multiple times.
///
/// In particular, we want to lookup as few things as possible to get the fewer
/// selectors the better, so this looks up by id, class, or looks at the list of
/// state/other attribute affecting selectors.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct InvalidationMap {
    /// A map from a given class name to all the selectors with that class
    /// selector.
    pub class_to_selector: IdOrClassDependencyMap,
    /// A map from a given id to all the selectors with that ID in the
    /// stylesheets currently applying to the document.
    pub id_to_selector: IdOrClassDependencyMap,
    /// A map of all the state dependencies.
    pub state_affecting_selectors: StateDependencyMap,
    /// A list of document state dependencies in the rules we represent.
    pub document_state_selectors: Vec<DocumentStateDependency>,
    /// A map of other attribute affecting selectors.
    pub other_attribute_affecting_selectors: AttributeDependencyMap,
}

/// A map to store all relative selector invalidations.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct RelativeSelectorInvalidationMap {
    /// Portion common to the normal invalidation map, except that this is for relative selectors and their inner selectors.
    pub map: InvalidationMap,
    /// Flag indicating if any relative selector is used.
    pub used: bool,
    /// Flag indicating if invalidating a relative selector requires ancestor traversal.
    pub needs_ancestors_traversal: bool,
}

impl RelativeSelectorInvalidationMap {
    /// Creates an empty `InvalidationMap`.
    pub fn new() -> Self {
        Self {
            map: InvalidationMap::new(),
            used: false,
            needs_ancestors_traversal: false,
        }
    }

    /// Returns the number of dependencies stored in the invalidation map.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Clears this map, leaving it empty.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Shrink the capacity of hash maps if needed.
    pub fn shrink_if_needed(&mut self) {
        self.map.shrink_if_needed();
    }
}

impl InvalidationMap {
    /// Creates an empty `InvalidationMap`.
    pub fn new() -> Self {
        Self {
            class_to_selector: IdOrClassDependencyMap::new(),
            id_to_selector: IdOrClassDependencyMap::new(),
            state_affecting_selectors: StateDependencyMap::new(),
            document_state_selectors: Vec::new(),
            other_attribute_affecting_selectors: AttributeDependencyMap::default(),
        }
    }

    /// Returns the number of dependencies stored in the invalidation map.
    pub fn len(&self) -> usize {
        self.state_affecting_selectors.len() +
            self.document_state_selectors.len() +
            self.other_attribute_affecting_selectors
                .iter()
                .fold(0, |accum, (_, ref v)| accum + v.len()) +
            self.id_to_selector
                .iter()
                .fold(0, |accum, (_, ref v)| accum + v.len()) +
            self.class_to_selector
                .iter()
                .fold(0, |accum, (_, ref v)| accum + v.len())
    }

    /// Clears this map, leaving it empty.
    pub fn clear(&mut self) {
        self.class_to_selector.clear();
        self.id_to_selector.clear();
        self.state_affecting_selectors.clear();
        self.document_state_selectors.clear();
        self.other_attribute_affecting_selectors.clear();
    }

    /// Shrink the capacity of hash maps if needed.
    pub fn shrink_if_needed(&mut self) {
        self.class_to_selector.shrink_if_needed();
        self.id_to_selector.shrink_if_needed();
        self.state_affecting_selectors.shrink_if_needed();
        self.other_attribute_affecting_selectors.shrink_if_needed();
    }
}

/// Adds a selector to the given `InvalidationMap`. Returns Err(..) to signify OOM.
pub fn note_selector_for_invalidation(
    selector: &Selector<SelectorImpl>,
    quirks_mode: QuirksMode,
    map: &mut InvalidationMap,
    relative_selector_invalidation_map: &mut RelativeSelectorInvalidationMap,
) -> Result<(), AllocErr> {
    debug!("note_selector_for_invalidation({:?})", selector);

    let mut document_state = DocumentState::empty();
    {
        let mut parent_stack = ParentSelectors::new();
        let mut alloc_error = None;
        let mut collector = SelectorDependencyCollector {
            map,
            relative_selector_invalidation_map,
            document_state: &mut document_state,
            selector,
            parent_selectors: &mut parent_stack,
            quirks_mode,
            compound_state: PerCompoundState::new(0),
            alloc_error: &mut alloc_error,
        };

        let visit_result = collector.visit_whole_selector();
        debug_assert_eq!(!visit_result, alloc_error.is_some());
        if let Some(alloc_error) = alloc_error {
            return Err(alloc_error);
        }
    }

    if !document_state.is_empty() {
        let dep = DocumentStateDependency {
            state: document_state,
            dependency: Dependency::for_full_selector_invalidation(selector.clone()),
        };
        map.document_state_selectors.try_reserve(1)?;
        map.document_state_selectors.push(dep);
    }
    Ok(())
}
struct PerCompoundState {
    /// The offset at which our compound starts.
    offset: usize,

    /// The state this compound selector is affected by.
    element_state: ElementState,
}

impl PerCompoundState {
    fn new(offset: usize) -> Self {
        Self {
            offset,
            element_state: ElementState::empty(),
        }
    }
}

struct ParentDependencyEntry {
    selector: Selector<SelectorImpl>,
    offset: usize,
    cached_dependency: Option<Arc<Dependency>>,
}

trait Collector {
    fn dependency(&mut self) -> Dependency;
    fn id_map(&mut self) -> &mut IdOrClassDependencyMap;
    fn class_map(&mut self) -> &mut IdOrClassDependencyMap;
    fn state_map(&mut self) -> &mut StateDependencyMap;
    fn attribute_map(&mut self) -> &mut AttributeDependencyMap;
    fn update_states(&mut self, element_state: ElementState, document_state: DocumentState);
}

fn on_attribute<C: Collector>(
    local_name: &LocalName,
    local_name_lower: &LocalName,
    collector: &mut C,
) -> Result<(), AllocErr> {
    add_attr_dependency(local_name.clone(), collector)?;

    if local_name != local_name_lower {
        add_attr_dependency(local_name_lower.clone(), collector)?;
    }
    Ok(())
}

fn on_id_or_class<C: Collector>(
    s: &Component<SelectorImpl>,
    quirks_mode: QuirksMode,
    collector: &mut C,
) -> Result<(), AllocErr> {
    let dependency = collector.dependency();
    let (atom, map) = match *s {
        Component::ID(ref atom) => (atom, collector.id_map()),
        Component::Class(ref atom) => (atom, collector.class_map()),
        _ => unreachable!(),
    };
    let entry = map.try_entry(atom.0.clone(), quirks_mode)?;
    let vec = entry.or_insert_with(SmallVec::new);
    vec.try_reserve(1)?;
    vec.push(dependency);
    Ok(())
}

fn add_attr_dependency<C: Collector>(name: LocalName, collector: &mut C) -> Result<(), AllocErr> {
    let dependency = collector.dependency();
    let map = collector.attribute_map();
    map.try_reserve(1)?;
    let vec = map.entry(name).or_default();
    vec.try_reserve(1)?;
    vec.push(dependency);
    Ok(())
}

fn on_pseudo_class<C: Collector>(pc: &NonTSPseudoClass, collector: &mut C) -> Result<(), AllocErr> {
    collector.update_states(pc.state_flag(), pc.document_state_flag());

    let attr_name = match *pc {
        #[cfg(feature = "gecko")]
        NonTSPseudoClass::MozTableBorderNonzero => local_name!("border"),
        #[cfg(feature = "gecko")]
        NonTSPseudoClass::MozBrowserFrame => local_name!("mozbrowser"),
        #[cfg(feature = "gecko")]
        NonTSPseudoClass::MozSelectListBox => {
            // This depends on two attributes.
            add_attr_dependency(local_name!("multiple"), collector)?;
            return add_attr_dependency(local_name!("size"), collector);
        },
        NonTSPseudoClass::Lang(..) => local_name!("lang"),
        _ => return Ok(()),
    };

    add_attr_dependency(attr_name, collector)
}

fn add_pseudo_class_dependency<C: Collector>(
    element_state: ElementState,
    quirks_mode: QuirksMode,
    collector: &mut C,
) -> Result<(), AllocErr> {
    if element_state.is_empty() {
        return Ok(());
    }
    let dependency = collector.dependency();
    collector.state_map().insert(
        StateDependency {
            dep: dependency,
            state: element_state,
        },
        quirks_mode,
    )
}

type ParentSelectors = SmallVec<[ParentDependencyEntry; 5]>;

/// A struct that collects invalidations for a given compound selector.
struct SelectorDependencyCollector<'a> {
    map: &'a mut InvalidationMap,
    relative_selector_invalidation_map: &'a mut RelativeSelectorInvalidationMap,

    /// The document this _complex_ selector is affected by.
    ///
    /// We don't need to track state per compound selector, since it's global
    /// state and it changes for everything.
    document_state: &'a mut DocumentState,

    /// The current selector and offset we're iterating.
    selector: &'a Selector<SelectorImpl>,

    /// The stack of parent selectors that we have, and at which offset of the
    /// sequence.
    ///
    /// This starts empty. It grows when we find nested :is and :where selector
    /// lists. The dependency field is cached and reference counted.
    parent_selectors: &'a mut ParentSelectors,

    /// The quirks mode of the document where we're inserting dependencies.
    quirks_mode: QuirksMode,

    /// State relevant to a given compound selector.
    compound_state: PerCompoundState,

    /// The allocation error, if we OOM.
    alloc_error: &'a mut Option<AllocErr>,
}

fn parent_dependency(parent_selectors: &mut ParentSelectors) -> Option<Arc<Dependency>> {
    if parent_selectors.is_empty() {
        return None;
    }

    fn dependencies_from(entries: &mut [ParentDependencyEntry]) -> Option<Arc<Dependency>> {
        if entries.is_empty() {
            return None;
        }

        let last_index = entries.len() - 1;
        let (previous, last) = entries.split_at_mut(last_index);
        let last = &mut last[0];
        let selector = &last.selector;
        let selector_offset = last.offset;
        Some(
            last.cached_dependency
                .get_or_insert_with(|| {
                    Arc::new(Dependency {
                        selector: selector.clone(),
                        selector_offset,
                        parent: dependencies_from(previous),
                        relative_kind: None,
                    })
                })
                .clone(),
        )
    }

    dependencies_from(parent_selectors)
}

impl<'a> Collector for SelectorDependencyCollector<'a> {
    fn dependency(&mut self) -> Dependency {
        let parent = parent_dependency(self.parent_selectors);
        Dependency {
            selector: self.selector.clone(),
            selector_offset: self.compound_state.offset,
            parent,
            relative_kind: None,
        }
    }

    fn id_map(&mut self) -> &mut IdOrClassDependencyMap {
        &mut self.map.id_to_selector
    }

    fn class_map(&mut self) -> &mut IdOrClassDependencyMap {
        &mut self.map.class_to_selector
    }

    fn state_map(&mut self) -> &mut StateDependencyMap {
        &mut self.map.state_affecting_selectors
    }

    fn attribute_map(&mut self) -> &mut AttributeDependencyMap {
        &mut self.map.other_attribute_affecting_selectors
    }

    fn update_states(&mut self, element_state: ElementState, document_state: DocumentState) {
        self.compound_state.element_state |= element_state;
        *self.document_state |= document_state;
    }
}

impl<'a> SelectorDependencyCollector<'a> {
    fn visit_whole_selector(&mut self) -> bool {
        let iter = self.selector.iter();
        self.visit_whole_selector_from(iter, 0)
    }

    fn visit_whole_selector_from(
        &mut self,
        mut iter: SelectorIter<SelectorImpl>,
        mut index: usize,
    ) -> bool {
        loop {
            // Reset the compound state.
            self.compound_state = PerCompoundState::new(index);

            // Visit all the simple selectors in this sequence.
            for ss in &mut iter {
                if !ss.visit(self) {
                    return false;
                }
                index += 1; // Account for the simple selector.
            }

            if let Err(err) = add_pseudo_class_dependency(
                self.compound_state.element_state,
                self.quirks_mode,
                self,
            ) {
                *self.alloc_error = Some(err);
                return false;
            }

            let combinator = iter.next_sequence();
            if combinator.is_none() {
                return true;
            }
            index += 1; // account for the combinator
        }
    }
}

impl<'a> SelectorVisitor for SelectorDependencyCollector<'a> {
    type Impl = SelectorImpl;

    fn visit_selector_list(
        &mut self,
        _list_kind: SelectorListKind,
        list: &[Selector<SelectorImpl>],
    ) -> bool {
        for selector in list {
            // Here we cheat a bit: We can visit the rightmost compound with
            // the "outer" visitor, and it'd be fine. This reduces the amount of
            // state and attribute invalidations, and we need to check the outer
            // selector to the left anyway to avoid over-invalidation, so it
            // avoids matching it twice uselessly.
            let mut iter = selector.iter();
            let mut index = 0;

            for ss in &mut iter {
                if !ss.visit(self) {
                    return false;
                }
                index += 1;
            }

            let combinator = iter.next_sequence();
            if combinator.is_none() {
                continue;
            }

            index += 1; // account for the combinator.

            self.parent_selectors.push(ParentDependencyEntry {
                selector: self.selector.clone(),
                offset: self.compound_state.offset,
                cached_dependency: None,
            });
            let mut nested = SelectorDependencyCollector {
                map: &mut *self.map,
                relative_selector_invalidation_map: &mut *self.relative_selector_invalidation_map,
                document_state: &mut *self.document_state,
                selector,
                parent_selectors: &mut *self.parent_selectors,
                quirks_mode: self.quirks_mode,
                compound_state: PerCompoundState::new(index),
                alloc_error: &mut *self.alloc_error,
            };
            if !nested.visit_whole_selector_from(iter, index) {
                return false;
            }
            self.parent_selectors.pop();
        }
        true
    }

    fn visit_relative_selector_list(
        &mut self,
        list: &[selectors::parser::RelativeSelector<Self::Impl>],
    ) -> bool {
        self.relative_selector_invalidation_map.used = true;
        for relative_selector in list {
            // We can't cheat here like we do with other selector lists - the rightmost
            // compound of a relative selector is not the subject of the invalidation.
            self.parent_selectors.push(ParentDependencyEntry {
                selector: self.selector.clone(),
                offset: self.compound_state.offset,
                cached_dependency: None,
            });
            let mut nested = RelativeSelectorDependencyCollector {
                map: &mut *self.relative_selector_invalidation_map,
                document_state: &mut *self.document_state,
                selector: &relative_selector,
                combinator_count: RelativeSelectorCombinatorCount::new(relative_selector),
                parent_selectors: &mut *self.parent_selectors,
                quirks_mode: self.quirks_mode,
                compound_state: PerCompoundState::new(0),
                alloc_error: &mut *self.alloc_error,
            };
            if !nested.visit_whole_selector() {
                return false;
            }
            self.parent_selectors.pop();
        }
        true
    }

    fn visit_simple_selector(&mut self, s: &Component<SelectorImpl>) -> bool {
        match *s {
            Component::ID(..) | Component::Class(..) => {
                if let Err(err) = on_id_or_class(s, self.quirks_mode, self) {
                    *self.alloc_error = Some(err.into());
                    return false;
                }
                true
            },
            Component::NonTSPseudoClass(ref pc) => {
                if let Err(err) = on_pseudo_class(pc, self) {
                    *self.alloc_error = Some(err.into());
                    return false;
                }
                true
            },
            _ => true,
        }
    }

    fn visit_attribute_selector(
        &mut self,
        _: &NamespaceConstraint<&Namespace>,
        local_name: &LocalName,
        local_name_lower: &LocalName,
    ) -> bool {
        if let Err(err) = on_attribute(local_name, local_name_lower, self) {
            *self.alloc_error = Some(err);
            return false;
        }
        true
    }
}

/// A struct that collects invalidations for a given compound selector.
struct RelativeSelectorDependencyCollector<'a> {
    map: &'a mut RelativeSelectorInvalidationMap,

    /// The document this _complex_ selector is affected by.
    ///
    /// We don't need to track state per compound selector, since it's global
    /// state and it changes for everything.
    document_state: &'a mut DocumentState,

    /// The current inner relative selector and offset we're iterating.
    selector: &'a RelativeSelector<SelectorImpl>,
    /// Running combinator for this inner relative selector.
    combinator_count: RelativeSelectorCombinatorCount,

    /// The stack of parent selectors that we have, and at which offset of the
    /// sequence.
    ///
    /// This starts empty. It grows when we find nested :is and :where selector
    /// lists. The dependency field is cached and reference counted.
    parent_selectors: &'a mut ParentSelectors,

    /// The quirks mode of the document where we're inserting dependencies.
    quirks_mode: QuirksMode,

    /// State relevant to a given compound selector.
    compound_state: PerCompoundState,

    /// The allocation error, if we OOM.
    alloc_error: &'a mut Option<AllocErr>,
}

impl<'a> RelativeSelectorDependencyCollector<'a> {
    fn visit_whole_selector(&mut self) -> bool {
        let mut iter = self.selector.selector.iter_skip_relative_selector_anchor();
        let mut index = 0;

        self.map.needs_ancestors_traversal |= match self.selector.match_hint {
            RelativeSelectorMatchHint::InNextSiblingSubtree |
            RelativeSelectorMatchHint::InSiblingSubtree |
            RelativeSelectorMatchHint::InSubtree => true,
            _ => false,
        };
        loop {
            // Reset the compound state.
            self.compound_state = PerCompoundState::new(index);

            // Visit all the simple selectors in this sequence.
            for ss in &mut iter {
                if !ss.visit(self) {
                    return false;
                }
                index += 1; // Account for the simple selector.
            }

            if let Err(err) = add_pseudo_class_dependency(
                self.compound_state.element_state,
                self.quirks_mode,
                self,
            ) {
                *self.alloc_error = Some(err);
                return false;
            }

            let combinator = iter.next_sequence();
            if let Some(c) = combinator {
                match c {
                    Combinator::Child | Combinator::Descendant => {
                        self.combinator_count.child_or_descendants -= 1
                    },
                    Combinator::NextSibling | Combinator::LaterSibling => {
                        self.combinator_count.adjacent_or_next_siblings -= 1
                    },
                    Combinator::Part | Combinator::PseudoElement | Combinator::SlotAssignment => (),
                }
            } else {
                return true;
            }
            index += 1; // account for the combinator
        }
    }
}

impl<'a> Collector for RelativeSelectorDependencyCollector<'a> {
    fn dependency(&mut self) -> Dependency {
        let parent = parent_dependency(self.parent_selectors);
        Dependency {
            selector: self.selector.selector.clone(),
            selector_offset: self.compound_state.offset,
            relative_kind: Some(match self.combinator_count.get_match_hint() {
                RelativeSelectorMatchHint::InChild => RelativeDependencyInvalidationKind::Parent,
                RelativeSelectorMatchHint::InSubtree => RelativeDependencyInvalidationKind::Ancestors,
                RelativeSelectorMatchHint::InNextSibling => {
                    RelativeDependencyInvalidationKind::PrevSibling
                },
                RelativeSelectorMatchHint::InSibling => {
                    RelativeDependencyInvalidationKind::EarlierSibling
                },
                RelativeSelectorMatchHint::InNextSiblingSubtree => {
                    RelativeDependencyInvalidationKind::AncestorPrevSibling
                },
                RelativeSelectorMatchHint::InSiblingSubtree => {
                    RelativeDependencyInvalidationKind::AncestorEarlierSibling
                },
            }),
            parent,
        }
    }

    fn id_map(&mut self) -> &mut IdOrClassDependencyMap {
        &mut self.map.map.id_to_selector
    }

    fn class_map(&mut self) -> &mut IdOrClassDependencyMap {
        &mut self.map.map.class_to_selector
    }

    fn state_map(&mut self) -> &mut StateDependencyMap {
        &mut self.map.map.state_affecting_selectors
    }

    fn attribute_map(&mut self) -> &mut AttributeDependencyMap {
        &mut self.map.map.other_attribute_affecting_selectors
    }

    fn update_states(&mut self, element_state: ElementState, document_state: DocumentState) {
        self.compound_state.element_state |= element_state;
        *self.document_state |= document_state;
    }
}

impl<'a> SelectorVisitor for RelativeSelectorDependencyCollector<'a> {
    type Impl = SelectorImpl;

    fn visit_selector_list(
        &mut self,
        _list_kind: SelectorListKind,
        _list: &[Selector<SelectorImpl>],
    ) -> bool {
        // TODO(dshin): Inner selector invalidation.
        true
    }

    fn visit_relative_selector_list(
        &mut self,
        _list: &[selectors::parser::RelativeSelector<Self::Impl>],
    ) -> bool {
        unreachable!("Nested relative selector?");
    }

    fn visit_simple_selector(&mut self, s: &Component<SelectorImpl>) -> bool {
        match *s {
            Component::ID(..) | Component::Class(..) => {
                if let Err(err) = on_id_or_class(s, self.quirks_mode, self) {
                    *self.alloc_error = Some(err.into());
                    return false;
                }
                true
            },
            Component::NonTSPseudoClass(ref pc) => {
                if let Err(err) = on_pseudo_class(pc, self) {
                    *self.alloc_error = Some(err.into());
                    return false;
                }
                true
            },
            _ => true,
        }
    }

    fn visit_attribute_selector(
        &mut self,
        _: &NamespaceConstraint<&Namespace>,
        local_name: &LocalName,
        local_name_lower: &LocalName,
    ) -> bool {
        if let Err(err) = on_attribute(local_name, local_name_lower, self) {
            *self.alloc_error = Some(err);
            return false;
        }
        true
    }
}
