//! Current-page keymap overlay (R-18 Help).

use ratatui::text::{Line, Span};

use crate::state::PageId;

const PAD: &str = "  ";

/// Title line for the help overlay.
#[must_use]
pub fn title(page: PageId) -> String {
    format!("{PAD}Help — {}", page.slug())
}

/// Key descriptions for `page`, list mode only.
#[must_use]
pub fn entries(page: PageId) -> Vec<&'static str> {
    match page {
        PageId::Agents => vec![
            "↑↓ j/k     next or previous agent",
            "←→ h/l     cycle model, visibility, connection",
            "↵ space    field picker",
            "S          sync public model catalog",
            "s / p      save or switch profiles",
            "r          reload",
            "1–6 ] [    change page",
            "?          this help",
            "q esc      quit",
        ],
        PageId::Providers => vec![
            "↑↓ j/k     provider row",
            "↵ space    model list",
            "e w a f u o  edit provider fields",
            "t          connectivity test",
            "m          refresh models",
            "b          balances",
            "d d        delete provider",
            "r          reload",
            "1–6 ] [    change page",
            "?          this help",
            "q esc      quit",
        ],
        PageId::Routing => vec![
            "↑↓ j/k     group row",
            "↵ space    open group",
            "n          new group",
            "o          next routing strategy",
            "u          restore hidden group",
            "d d        delete group",
            "r          reload",
            "1–6 ] [    change page",
            "?          this help",
            "q esc      quit",
        ],
        PageId::Usage => vec![
            "←→ h/l     reporting period",
            "t w m A    jump period",
            "u          used versus left quota display",
            "c          run daily check-ins",
            "a / b      edit usage or balance alerts",
            "r          reload",
            "1–6 ] [    change page",
            "?          this help",
            "q esc      quit",
        ],
        PageId::Sessions => vec![
            "↑↓ j/k     session row",
            "↵          resume in this terminal",
            "←→ h/l     time range",
            "s tab      list versus stats",
            "M f        model or folder filter",
            "x          clear filters",
            "r          reload",
            "1–6 ] [    change page",
            "?          this help",
            "q esc      quit",
        ],
        PageId::Library => vec![
            "↑↓ j/k     library row",
            "r          reload",
            "1–6 ] [    change page",
            "?          this help",
            "q esc      quit",
        ],
        PageId::Accounts => vec![
            "↑↓ j/k     account row",
            "a          start sign-in",
            "↵          switch agent sign-in",
            "n          next account with room",
            "i / f      import or discover",
            "d d        remove account",
            "x          refresh tokens",
            "7 9        accounts or logs page",
            "r          reload",
            "?          this help",
            "q esc      quit",
        ],
        PageId::Logs => vec![
            "↑↓ j/k     scroll log lines",
            "f          follow (poll every 1s)",
            "e          toggle request lines",
            "c          clear view (not the file)",
            "9          logs page",
            "r          tail again",
            "1–6 ] [    change page",
            "?          this help",
            "q esc      quit",
        ],
    }
}

/// Renders help lines with muted body text.
#[must_use]
pub fn lines(
    page: PageId,
    title_style: ratatui::style::Style,
    muted: ratatui::style::Style,
) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(Span::styled(title(page), title_style)),
        Line::from(""),
    ];
    for entry in entries(page) {
        out.push(Line::from(Span::styled(format!("{PAD}{entry}"), muted)));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        format!("{PAD}Press esc to return."),
        muted,
    )));
    out
}
