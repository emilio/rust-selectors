/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
use std::borrow::Borrow;

use bloom::BloomFilter;

use parser::{CaseSensitivity, Combinator, ComplexSelector, LocalName};
use parser::{SimpleSelector, Selector, SelectorImpl};
use tree::Element;

// The bloom filter for descendant CSS selectors will have a <1% false
// positive rate until it has this many selectors in it, then it will
// rapidly increase.
pub static RECOMMENDED_SELECTOR_BLOOM_FILTER_SIZE: usize = 4096;

bitflags! {
    /// Set of flags that determine the different kind of elements affected by
    /// the selector matching process.
    ///
    /// This is used to implement efficient sharing.
    pub flags StyleRelations: u32 {
        /// Whether this element has matched any rule that is determined by a
        /// sibling (when using the `+` or `~` combinators).
        const AFFECTED_BY_SIBLINGS = 1 << 0,

        /// Whether this flag is affected by any state (i.e., non
        /// tree-structural pseudo-class).
        const AFFECTED_BY_STATE = 1 << 1,

        /// Whether this element is affected by an ID selector.
        const AFFECTED_BY_ID_SELECTOR = 1 << 2,

        /// Whether this element is affected by a non-common style-affecting
        /// attribute.
        const AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR = 1 << 3,

        /// Whether this element matches the :empty pseudo class.
        const AFFECTED_BY_EMPTY = 1 << 4,

        /// Whether this element has a style attribute. Computed
        /// externally.
        const AFFECTED_BY_STYLE_ATTRIBUTE = 1 << 5,

        /// Whether this element is affected by presentational hints. This is
        /// computed externally (that is, in Servo).
        const AFFECTED_BY_PRESENTATIONAL_HINTS = 1 << 6,

        /// Whether this element has pseudo-element styles. Computed externally.
        const AFFECTED_BY_PSEUDO_ELEMENTS = 1 << 7,

        /// :nth-of-type
        const AFFECTED_BY_NTH_OF_TYPE = 1 << 8,

        /// :nth-last-of-type
        const AFFECTED_BY_NTH_LAST_OF_TYPE = 1 << 9,

        /// :first-of-type
        const AFFECTED_BY_FIRST_OF_TYPE = 1 << 10,

        /// :last-of-type
        const AFFECTED_BY_LAST_OF_TYPE = 1 << 11,

        /// :only-of-type
        const AFFECTED_BY_ONLY_OF_TYPE = 1 << 12,

        /// :nth-child
        const AFFECTED_BY_NTH_CHILD = 1 << 13,

        /// :nth-last-child
        const AFFECTED_BY_NTH_LAST_CHILD = 1 << 14,

        /// :first-child
        const AFFECTED_BY_FIRST_CHILD = 1 << 15,

        /// :last-child
        const AFFECTED_BY_LAST_CHILD = 1 << 16,

        /// :only-child
        const AFFECTED_BY_ONLY_CHILD = 1 << 17,
    }
}

impl StyleRelations {
    #[inline]
    pub fn affected_by_child_index(&self) -> bool {
        self.intersects(AFFECTED_BY_NTH_OF_TYPE | AFFECTED_BY_NTH_LAST_OF_TYPE |
                        AFFECTED_BY_FIRST_OF_TYPE | AFFECTED_BY_LAST_OF_TYPE |
                        AFFECTED_BY_NTH_CHILD | AFFECTED_BY_NTH_LAST_CHILD |
                        AFFECTED_BY_FIRST_CHILD | AFFECTED_BY_LAST_CHILD |
                        AFFECTED_BY_ONLY_OF_TYPE | AFFECTED_BY_ONLY_CHILD)
    }
}

pub fn matches<E>(selector_list: &[Selector<E::Impl>],
                  element: &E,
                  parent_bf: Option<&BloomFilter>)
                  -> bool
                  where E: Element {
    selector_list.iter().any(|selector| {
        selector.pseudo_element.is_none() &&
        matches_complex_selector(&*selector.complex_selector, element, parent_bf, &mut StyleRelations::empty())
    })
}

/// Determines whether the given element matches the given complex selector.
///
/// NB: If you add support for any new kinds of selectors to this routine, be sure to set
/// `shareable` to false unless you are willing to update the style sharing logic. Otherwise things
/// will almost certainly break as elements will start mistakenly sharing styles. (See
/// `can_share_style_with` in `servo/components/style/matching.rs`.)
pub fn matches_complex_selector<E>(selector: &ComplexSelector<E::Impl>,
                                   element: &E,
                                   parent_bf: Option<&BloomFilter>,
                                   relations: &mut StyleRelations)
                                   -> bool
    where E: Element
{
    match matches_complex_selector_internal(selector, element, parent_bf, relations) {
        SelectorMatchingResult::Matched => {
            match selector.next {
                Some((_, Combinator::NextSibling)) |
                Some((_, Combinator::LaterSibling)) => *relations |= AFFECTED_BY_SIBLINGS,
                _ => {}
            }

            true
        }
        _ => false
    }
}

/// A result of selector matching, includes 3 failure types,
///
///   NotMatchedAndRestartFromClosestLaterSibling
///   NotMatchedAndRestartFromClosestDescendant
///   NotMatchedGlobally
///
/// When NotMatchedGlobally appears, stop selector matching completely since
/// the succeeding selectors never matches.
/// It is raised when
///   Child combinator cannot find the candidate element.
///   Descendant combinator cannot find the candidate element.
///
/// When NotMatchedAndRestartFromClosestDescendant appears, the selector
/// matching does backtracking and restarts from the closest Descendant
/// combinator.
/// It is raised when
///   NextSibling combinator cannot find the candidate element.
///   LaterSibling combinator cannot find the candidate element.
///   Child combinator doesn't match on the found element.
///
/// When NotMatchedAndRestartFromClosestLaterSibling appears, the selector
/// matching does backtracking and restarts from the closest LaterSibling
/// combinator.
/// It is raised when
///   NextSibling combinator doesn't match on the found element.
///
/// For example, when the selector "d1 d2 a" is provided and we cannot *find*
/// an appropriate ancestor element for "d1", this selector matching raises
/// NotMatchedGlobally since even if "d2" is moved to more upper element, the
/// candidates for "d1" becomes less than before and d1 .
///
/// The next example is siblings. When the selector "b1 + b2 ~ d1 a" is
/// provided and we cannot *find* an appropriate brother element for b1,
/// the selector matching raises NotMatchedAndRestartFromClosestDescendant.
/// The selectors ("b1 + b2 ~") doesn't match and matching restart from "d1".
///
/// The additional example is child and sibling. When the selector
/// "b1 + c1 > b2 ~ d1 a" is provided and the selector "b1" doesn't match on
/// the element, this "b1" raises NotMatchedAndRestartFromClosestLaterSibling.
/// However since the selector "c1" raises
/// NotMatchedAndRestartFromClosestDescendant. So the selector
/// "b1 + c1 > b2 ~ " doesn't match and restart matching from "d1".
#[derive(PartialEq, Eq, Copy, Clone)]
enum SelectorMatchingResult {
    Matched,
    NotMatchedAndRestartFromClosestLaterSibling,
    NotMatchedAndRestartFromClosestDescendant,
    NotMatchedGlobally,
}

/// Quickly figures out whether or not the complex selector is worth doing more
/// work on. If the simple selectors don't match, or there's a child selector
/// that does not appear in the bloom parent bloom filter, we can exit early.
fn can_fast_reject<E>(mut selector: &ComplexSelector<E::Impl>,
                      element: &E,
                      parent_bf: Option<&BloomFilter>,
                      relations: &mut StyleRelations)
                      -> Option<SelectorMatchingResult>
    where E: Element
{
    if !selector.compound_selector.iter().all(|simple_selector| {
      matches_simple_selector(simple_selector, element, parent_bf, relations) }) {
        return Some(SelectorMatchingResult::NotMatchedAndRestartFromClosestLaterSibling);
    }

    let bf: &BloomFilter = match parent_bf {
        None => return None,
        Some(ref bf) => bf,
    };

    // See if the bloom filter can exclude any of the descendant selectors, and
    // reject if we can.
    loop {
         match selector.next {
             None => break,
             Some((ref cs, Combinator::Descendant)) => selector = &**cs,
             Some((ref cs, _)) => {
                 selector = &**cs;
                 continue;
             }
         };

        for ss in selector.compound_selector.iter() {
            match *ss {
                SimpleSelector::LocalName(LocalName { ref name, ref lower_name })  => {
                    if !bf.might_contain(name)
                    && !bf.might_contain(lower_name) {
                        return Some(SelectorMatchingResult::NotMatchedGlobally);
                    }
                },
                SimpleSelector::Namespace(ref namespace) => {
                    if !bf.might_contain(&namespace.url) {
                        return Some(SelectorMatchingResult::NotMatchedGlobally);
                    }
                },
                SimpleSelector::ID(ref id) => {
                    if !bf.might_contain(id) {
                        return Some(SelectorMatchingResult::NotMatchedGlobally);
                    }
                },
                SimpleSelector::Class(ref class) => {
                    if !bf.might_contain(class) {
                        return Some(SelectorMatchingResult::NotMatchedGlobally);
                    }
                },
                _ => {},
            }
        }
    }

    // Can't fast reject.
    None
}

fn matches_complex_selector_internal<E>(selector: &ComplexSelector<E::Impl>,
                                         element: &E,
                                         parent_bf: Option<&BloomFilter>,
                                         relations: &mut StyleRelations)
                                         -> SelectorMatchingResult
     where E: Element
{
    if let Some(result) = can_fast_reject(selector, element, parent_bf, relations) {
        return result;
    }

    match selector.next {
        None => SelectorMatchingResult::Matched,
        Some((ref next_selector, combinator)) => {
            let (siblings, candidate_not_found) = match combinator {
                Combinator::Child => (false, SelectorMatchingResult::NotMatchedGlobally),
                Combinator::Descendant => (false, SelectorMatchingResult::NotMatchedGlobally),
                Combinator::NextSibling => (true, SelectorMatchingResult::NotMatchedAndRestartFromClosestDescendant),
                Combinator::LaterSibling => (true, SelectorMatchingResult::NotMatchedAndRestartFromClosestDescendant),
            };
            let mut next_element = if siblings {
                element.prev_sibling_element()
            } else {
                element.parent_element()
            };
            loop {
                let element = match next_element {
                    None => return candidate_not_found,
                    Some(next_element) => next_element,
                };
                let result = matches_complex_selector_internal(&**next_selector,
                                                                &element,
                                                                parent_bf,
                                                                relations);
                match (result, combinator) {
                    // Return the status immediately.
                    (SelectorMatchingResult::Matched, _) => return result,
                    (SelectorMatchingResult::NotMatchedGlobally, _) => return result,

                    // Upgrade the failure status to
                    // NotMatchedAndRestartFromClosestDescendant.
                    (_, Combinator::Child) => return SelectorMatchingResult::NotMatchedAndRestartFromClosestDescendant,

                    // Return the status directly.
                    (_, Combinator::NextSibling) => return result,

                    // If the failure status is NotMatchedAndRestartFromClosestDescendant
                    // and combinator is Combinator::LaterSibling, give up this Combinator::LaterSibling matching
                    // and restart from the closest descendant combinator.
                    (SelectorMatchingResult::NotMatchedAndRestartFromClosestDescendant, Combinator::LaterSibling) => return result,

                    // The Combinator::Descendant combinator and the status is
                    // NotMatchedAndRestartFromClosestLaterSibling or
                    // NotMatchedAndRestartFromClosestDescendant,
                    // or the Combinator::LaterSibling combinator and the status is
                    // NotMatchedAndRestartFromClosestDescendant
                    // can continue to matching on the next candidate element.
                    _ => {},
                }
                next_element = if siblings {
                    element.prev_sibling_element()
                } else {
                    element.parent_element()
                };
            }
        }
    }
}

/// Determines whether the given element matches the given single selector.
///
/// NB: If you add support for any new kinds of selectors to this routine, be sure to set
/// `shareable` to false unless you are willing to update the style sharing logic. Otherwise things
/// will almost certainly break as elements will start mistakenly sharing styles. (See
/// `can_share_style_with` in `servo/components/style/matching.rs`.)
#[inline]
fn matches_simple_selector<E>(
        selector: &SimpleSelector<E::Impl>,
        element: &E,
        parent_bf: Option<&BloomFilter>,
        relations: &mut StyleRelations)
        -> bool
    where E: Element
{
    macro_rules! relation_if {
        ($ex:expr, $flag:expr) => {
            if $ex {
                *relations |= $flag;
                true
            } else {
                false
            }
        }
    }

    match *selector {
        SimpleSelector::LocalName(LocalName { ref name, ref lower_name }) => {
            let name = if element.is_html_element_in_html_document() { lower_name } else { name };
            element.get_local_name() == name.borrow()
        }
        SimpleSelector::Namespace(ref namespace) => {
            element.get_namespace() == namespace.url.borrow()
        }
        // TODO: case-sensitivity depends on the document type and quirks mode
        SimpleSelector::ID(ref id) => {
            relation_if!(element.get_id().map_or(false, |attr| attr == *id),
                         AFFECTED_BY_ID_SELECTOR)
        }
        SimpleSelector::Class(ref class) => {
            element.has_class(class)
        }
        SimpleSelector::AttrExists(ref attr) => {
            let matches = element.match_attr_has(attr);

            if matches && !E::Impl::attr_exists_selector_is_shareable(attr) {
                *relations |= AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR;
            }

            matches
        }
        SimpleSelector::AttrEqual(ref attr, ref value, case_sensitivity) => {
            let matches = match case_sensitivity {
                CaseSensitivity::CaseSensitive => element.match_attr_equals(attr, value),
                CaseSensitivity::CaseInsensitive => element.match_attr_equals_ignore_ascii_case(attr, value),
            };

            if matches && !E::Impl::attr_equals_selector_is_shareable(attr, value) {
                *relations |= AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR;
            }

            matches
        }
        SimpleSelector::AttrIncludes(ref attr, ref value) => {
            relation_if!(element.match_attr_includes(attr, value),
                         AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR)
        }
        SimpleSelector::AttrDashMatch(ref attr, ref value) => {
            relation_if!(element.match_attr_dash(attr, value),
                         AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR)
        }
        SimpleSelector::AttrPrefixMatch(ref attr, ref value) => {
            relation_if!(element.match_attr_prefix(attr, value),
                         AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR)
        }
        SimpleSelector::AttrSubstringMatch(ref attr, ref value) => {
            relation_if!(element.match_attr_substring(attr, value),
                         AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR)
        }
        SimpleSelector::AttrSuffixMatch(ref attr, ref value) => {
            relation_if!(element.match_attr_suffix(attr, value),
                         AFFECTED_BY_NON_COMMON_STYLE_AFFECTING_ATTRIBUTE_SELECTOR)
        }
        SimpleSelector::NonTSPseudoClass(ref pc) => {
            relation_if!(element.match_non_ts_pseudo_class(pc.clone()),
                         AFFECTED_BY_STATE)
        }
        SimpleSelector::Root => {
            // We never share styles with an element with no parent, so no point
            // in creating a new StyleRelation.
            element.is_root()
        }
        SimpleSelector::Empty => {
            relation_if!(element.is_empty(), AFFECTED_BY_EMPTY)
        }
        SimpleSelector::FirstChild => {
            relation_if!(matches_first_child(element),
                         AFFECTED_BY_FIRST_CHILD)
        }
        SimpleSelector::LastChild => {
            relation_if!(matches_last_child(element),
                         AFFECTED_BY_LAST_CHILD)
        }
        SimpleSelector::OnlyChild => {
            relation_if!(matches_first_child(element) && matches_last_child(element),
                         AFFECTED_BY_ONLY_CHILD)
        }
        SimpleSelector::NthChild(a, b) => {
            relation_if!(matches_generic_nth_child(element, a, b, false, false),
                         AFFECTED_BY_NTH_CHILD)
        }
        SimpleSelector::NthLastChild(a, b) => {
            relation_if!(matches_generic_nth_child(element, a, b, false, true),
                         AFFECTED_BY_NTH_LAST_CHILD)
        }
        SimpleSelector::NthOfType(a, b) => {
            relation_if!(matches_generic_nth_child(element, a, b, true, false),
                         AFFECTED_BY_NTH_OF_TYPE)
        }
        SimpleSelector::NthLastOfType(a, b) => {
            relation_if!(matches_generic_nth_child(element, a, b, true, true),
                         AFFECTED_BY_NTH_LAST_OF_TYPE)
        }
        SimpleSelector::FirstOfType => {
            relation_if!(matches_generic_nth_child(element, 0, 1, true, false),
                         AFFECTED_BY_FIRST_OF_TYPE)
        }
        SimpleSelector::LastOfType => {
            relation_if!(matches_generic_nth_child(element, 0, 1, true, true),
                         AFFECTED_BY_LAST_OF_TYPE)
        }
        SimpleSelector::OnlyOfType => {
            relation_if!(matches_generic_nth_child(element, 0, 1, true, false) &&
                         matches_generic_nth_child(element, 0, 1, true, true),
                         AFFECTED_BY_ONLY_OF_TYPE)
        }
        SimpleSelector::Negation(ref negated) => {
            !negated.iter().all(|s| {
                matches_complex_selector(s, element, parent_bf, relations)
            })
        }
    }
}

#[inline]
fn matches_generic_nth_child<E>(element: &E,
                                a: i32,
                                b: i32,
                                is_of_type: bool,
                                is_from_end: bool) -> bool
    where E: Element
{
    // Selectors Level 4 changed from Level 3:
    // This can match without a parent element:
    // https://drafts.csswg.org/selectors-4/#child-index

    let mut index = 1;
    let mut next_sibling = if is_from_end {
        element.next_sibling_element()
    } else {
        element.prev_sibling_element()
    };

    loop {
        let sibling = match next_sibling {
            None => break,
            Some(next_sibling) => next_sibling
        };

        if is_of_type {
            if element.get_local_name() == sibling.get_local_name() &&
                element.get_namespace() == sibling.get_namespace() {
                index += 1;
            }
        } else {
          index += 1;
        }
        next_sibling = if is_from_end {
            sibling.next_sibling_element()
        } else {
            sibling.prev_sibling_element()
        };
    }

    if a == 0 {
        b == index
    } else {
        (index - b) / a >= 0 &&
        (index - b) % a == 0
    }
}

#[inline]
fn matches_first_child<E>(element: &E) -> bool where E: Element {
    // Selectors Level 4 changed from Level 3:
    // This can match without a parent element:
    // https://drafts.csswg.org/selectors-4/#child-index
    element.prev_sibling_element().is_none()
}

#[inline]
fn matches_last_child<E>(element: &E) -> bool where E: Element {
    // Selectors Level 4 changed from Level 3:
    // This can match without a parent element:
    // https://drafts.csswg.org/selectors-4/#child-index
    element.next_sibling_element().is_none()
}
