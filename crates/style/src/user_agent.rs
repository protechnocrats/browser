//! Milestone 33: the built-in user-agent stylesheet (`user_agent.css`).

use css_parser::Stylesheet;
use std::sync::OnceLock;

/// The user-agent stylesheet's source.
pub const USER_AGENT_CSS: &str = include_str!("user_agent.css");

/// The user-agent stylesheet, parsed once per process. A host puts it
/// below the page's own rules with
/// [`Stylesheet::with_user_agent`](css_parser::Stylesheet::with_user_agent).
pub fn user_agent_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| css_parser::parse_stylesheet(USER_AGENT_CSS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{resolve_style, Length};
    use dom::Element;

    #[test]
    fn every_ua_declaration_is_one_the_engine_implements() {
        // The registry is process-wide and tests run in parallel, so only
        // entries naming the UA sheet's own properties are considered.
        diagnostics::unsupported::set_enabled(true);
        let sheet = user_agent_stylesheet();
        assert!(sheet.rules.len() > 10);
        let mut names = std::collections::HashSet::new();
        for rule in &sheet.rules {
            for decl in &rule.declarations {
                names.insert(decl.property.clone());
                let mut el = Element::new("div");
                el.set_attr("style", format!("{}: {}", decl.property, decl.value));
                let _ = resolve_style(&el, &[], None, &Stylesheet::default());
            }
        }
        let unsupported: Vec<_> = diagnostics::unsupported::snapshot()
            .into_iter()
            .filter(|(_, name, _)| names.contains(name) || names.iter().any(|n| name.starts_with(format!("{n}:").as_str())))
            .collect();
        assert!(unsupported.is_empty(), "the UA sheet uses unimplemented CSS: {unsupported:?}");
    }

    #[test]
    fn author_rules_beat_user_agent_rules_whatever_the_specificity() {
        let author = css_parser::parse_stylesheet("h1 { margin-top: 3px } p { margin-top: 5px }");
        let sheet = author.with_user_agent(&css_parser::parse_stylesheet("#x.y h1, p#x.y { margin-top: 99px }"));
        let mut p = Element::new("p");
        p.set_attr("id", "x");
        p.set_attr("class", "y");
        assert_eq!(resolve_style(&p, &[], None, &sheet).margin.top, Some(Length::px(5.0)));
        let h1 = Element::new("h1");
        assert_eq!(resolve_style(&h1, &[], None, &user_agent_stylesheet().clone().with_user_agent(&Stylesheet::default())).font_size, 32.0);
    }

    #[test]
    fn user_agent_important_beats_author_important() {
        let author = css_parser::parse_stylesheet("p { margin-top: 5px !important }");
        let sheet = author.with_user_agent(&css_parser::parse_stylesheet("p { margin-top: 7px !important }"));
        let mut p = Element::new("p");
        p.set_attr("style", "margin-top: 9px !important");
        assert_eq!(resolve_style(&p, &[], None, &sheet).margin.top, Some(Length::px(7.0)));
    }
}
