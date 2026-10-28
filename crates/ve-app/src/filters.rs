//! The Filters panel: a form over the Query text. Opening it reads the clauses it can show as
//! fields; every edit writes the whole text back in a fixed order, and clauses the form cannot
//! show (`|` across modifiers, `!`, repeats, other comparisons) stay verbatim in Name contains.

use chrono::{Datelike, Duration, Local, NaiveDate};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::calendar::Date;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::date_picker::{DatePicker, DatePickerEvent, DatePickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ve_core::Category;
use ve_core::query::{ClauseSource, WordSource, clauses};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    B,
    KB,
    MB,
    GB,
    TB,
}

impl Unit {
    const ALL: [Unit; 5] = [Unit::B, Unit::KB, Unit::MB, Unit::GB, Unit::TB];

    fn label(self) -> &'static str {
        match self {
            Unit::B => "B",
            Unit::KB => "KB",
            Unit::MB => "MB",
            Unit::GB => "GB",
            Unit::TB => "TB",
        }
    }

    /// The unit a Query size ends with; the parser accepts `k` and `kb` alike.
    fn parse(suffix: &str) -> Option<Unit> {
        Some(match suffix.to_ascii_lowercase().as_str() {
            "" | "b" => Unit::B,
            "k" | "kb" => Unit::KB,
            "m" | "mb" => Unit::MB,
            "g" | "gb" => Unit::GB,
            "t" | "tb" => Unit::TB,
            _ => return None,
        })
    }
}

/// One end of the size range: an amount as typed (empty for no bound) and its unit.
#[derive(Clone, PartialEq, Debug)]
pub struct Bound {
    pub amount: String,
    pub unit: Unit,
}

impl Bound {
    fn empty() -> Bound {
        Bound {
            amount: String::new(),
            unit: Unit::MB,
        }
    }

    /// Reads `1.5gb`; `None` when the text is not a plain size.
    fn parse(text: &str) -> Option<Bound> {
        let split = text
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(text.len());
        let (amount, suffix) = text.split_at(split);
        amount.parse::<f64>().ok()?;
        Some(Bound {
            amount: amount.to_owned(),
            unit: Unit::parse(suffix)?,
        })
    }

    /// The bound as Query text, or `None` when empty or not a number.
    fn term(&self) -> Option<String> {
        let amount = self.amount.trim();
        amount.parse::<f64>().ok()?;
        Some(format!(
            "{amount}{}",
            self.unit.label().to_ascii_lowercase()
        ))
    }

    fn invalid(&self) -> bool {
        let amount = self.amount.trim();
        !amount.is_empty() && amount.parse::<f64>().is_err()
    }
}

/// A field with the checkbox that decides whether it is written.
#[derive(Clone, PartialEq, Debug)]
pub struct Field<T> {
    pub on: bool,
    pub value: T,
}

type Dates = (Option<NaiveDate>, Option<NaiveDate>);

#[derive(Clone, PartialEq, Debug)]
pub struct Filters {
    /// Plain name text, plus every clause the other fields cannot show, as typed.
    pub name: Field<String>,
    /// Extensions separated by `;`.
    pub ext: Field<String>,
    pub categories: Field<Vec<Category>>,
    pub size: Field<(Bound, Bound)>,
    pub modified: Field<Dates>,
    pub regex: Field<String>,
}

impl Filters {
    /// The form for Query `text`.
    pub fn read(text: &str) -> Filters {
        let mut form = Filters {
            name: Field {
                on: true,
                value: String::new(),
            },
            ext: Field {
                on: false,
                value: String::new(),
            },
            categories: Field {
                on: false,
                value: Vec::new(),
            },
            size: Field {
                on: false,
                value: (Bound::empty(), Bound::empty()),
            },
            modified: Field {
                on: false,
                value: (None, None),
            },
            regex: Field {
                on: false,
                value: String::new(),
            },
        };
        for clause in clauses(text) {
            if !form.take(&clause) {
                if !form.name.value.is_empty() {
                    form.name.value.push(' ');
                }
                form.name.value.push_str(clause.source);
            }
        }
        form
    }

    /// Fills the field `clause` belongs to, unless that field is already filled or the clause
    /// says more than the field can.
    fn take(&mut self, clause: &ClauseSource) -> bool {
        let modifier = |m: &str| {
            clause
                .words
                .iter()
                .all(|w| !w.negated && w.modifier == Some(m))
        };
        let single = |m: &str| clause.words.len() == 1 && modifier(m);
        let value = &clause.words[0].value;
        if single("ext") && !self.ext.on {
            self.ext = Field {
                on: true,
                value: value.clone(),
            };
        } else if single("regex") && !self.regex.on {
            self.regex = Field {
                on: true,
                value: value.clone(),
            };
        } else if single("size") && !self.size.on {
            let Some(range) = size_range(value) else {
                return false;
            };
            self.size = Field {
                on: true,
                value: range,
            };
        } else if single("dm") && !self.modified.on {
            let Some(range) = date_range(value) else {
                return false;
            };
            self.modified = Field {
                on: true,
                value: range,
            };
        } else if modifier("type") && !self.categories.on {
            let parsed: Option<Vec<Category>> = clause
                .words
                .iter()
                .map(|w: &WordSource| Category::parse(&w.value))
                .collect();
            let Some(categories) = parsed else {
                return false;
            };
            self.categories = Field {
                on: true,
                value: categories,
            };
        } else {
            return false;
        }
        true
    }

    /// The Query text the form describes: name, ext, type, size, dm, regex.
    pub fn write(&self) -> String {
        let mut terms: Vec<String> = Vec::new();
        if self.name.on && !self.name.value.trim().is_empty() {
            terms.push(self.name.value.trim().to_owned());
        }
        terms.extend(self.terms());
        terms.join(" ")
    }

    /// The terms of every field except Name, which is what "Filters · N" counts.
    fn terms(&self) -> impl Iterator<Item = String> {
        let ext = (self.ext.on && !self.ext.value.trim().is_empty())
            .then(|| format!("ext:{}", quoted(self.ext.value.trim())));
        let types = (self.categories.on && !self.categories.value.is_empty()).then(|| {
            let words: Vec<String> = self
                .categories
                .value
                .iter()
                .map(|c| format!("type:{}", c.name().to_lowercase()))
                .collect();
            words.join("|")
        });
        let size = if self.size.on {
            match (self.size.value.0.term(), self.size.value.1.term()) {
                (Some(min), Some(max)) => Some(format!("size:{min}..{max}")),
                (Some(min), None) => Some(format!("size:>={min}")),
                (None, Some(max)) => Some(format!("size:<={max}")),
                (None, None) => None,
            }
        } else {
            None
        };
        let modified = if self.modified.on {
            match self.modified.value {
                (Some(from), Some(to)) => Some(format!("dm:{from}..{to}")),
                (Some(from), None) => Some(format!("dm:>={from}")),
                (None, Some(to)) => Some(format!("dm:<={to}")),
                (None, None) => None,
            }
        } else {
            None
        };
        let regex = (self.regex.on && !self.regex.value.is_empty())
            .then(|| format!("regex:{}", quoted(&self.regex.value)));
        [ext, types, size, modified, regex].into_iter().flatten()
    }

    /// How many fields other than Name add a term.
    pub fn active(&self) -> usize {
        self.terms().count()
    }

    /// Whether Name holds clauses that are more than plain words.
    fn name_has_query_syntax(&self) -> bool {
        clauses(&self.name.value)
            .iter()
            .any(|c| c.words.len() > 1 || c.words.iter().any(|w| w.negated || w.modifier.is_some()))
    }
}

/// A value that holds a space or a `|` is quoted so it stays one word.
fn quoted(value: &str) -> String {
    if value.contains(|c: char| c.is_whitespace() || c == '|') {
        format!("\"{value}\"")
    } else {
        value.to_owned()
    }
}

/// `>=a`, `<=b` or `a..b` as a size range; other comparisons stay text.
fn size_range(value: &str) -> Option<(Bound, Bound)> {
    if let Some(min) = value.strip_prefix(">=") {
        Some((Bound::parse(min)?, Bound::empty()))
    } else if let Some(max) = value.strip_prefix("<=") {
        Some((Bound::empty(), Bound::parse(max)?))
    } else {
        let (min, max) = value.split_once("..")?;
        Some((Bound::parse(min)?, Bound::parse(max)?))
    }
}

/// `>=d`, `<=d` or `d..d` with whole `YYYY-MM-DD` dates; other forms stay text.
fn date_range(value: &str) -> Option<Dates> {
    let date = |text: &str| -> Option<NaiveDate> {
        (text.len() == 10).then_some(())?;
        text.parse().ok()
    };
    if let Some(from) = value.strip_prefix(">=") {
        Some((Some(date(from)?), None))
    } else if let Some(to) = value.strip_prefix("<=") {
        Some((None, Some(date(to)?)))
    } else {
        let (from, to) = value.split_once("..")?;
        Some((Some(date(from)?), Some(date(to)?)))
    }
}

/// The panel: the form plus the widgets that edit it. Emits the new Query text on every edit;
/// the window puts it in the Query bar and the Engine.
pub struct FiltersPanel {
    form: Filters,
    name: Entity<InputState>,
    ext: Entity<InputState>,
    regex: Entity<InputState>,
    min: Entity<InputState>,
    max: Entity<InputState>,
    dates: Entity<DatePickerState>,
}

impl EventEmitter<String> for FiltersPanel {}

impl FiltersPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = |hint: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(hint))
        };
        let panel = Self {
            form: Filters::read(""),
            name: text("part of a name", window, cx),
            ext: text("mp4;mkv", window, cx),
            regex: text("matches the name", window, cx),
            min: text("—", window, cx),
            max: text("—", window, cx),
            dates: cx.new(|cx| DatePickerState::range(window, cx)),
        };
        panel.watch(&panel.name, |f, v| f.name.value = v, cx);
        panel.watch(&panel.ext, |f, v| f.ext = Field { on: true, value: v }, cx);
        panel.watch(
            &panel.regex,
            |f, v| f.regex = Field { on: true, value: v },
            cx,
        );
        panel.watch(
            &panel.min,
            |f, v| {
                f.size.value.0.amount = v;
                f.size.on = true;
            },
            cx,
        );
        panel.watch(
            &panel.max,
            |f, v| {
                f.size.value.1.amount = v;
                f.size.on = true;
            },
            cx,
        );
        cx.subscribe(&panel.dates, |this, _, event: &DatePickerEvent, cx| {
            let DatePickerEvent::Change(range) = event;
            let day = |d: Option<chrono::NaiveDateTime>| d.map(|d| d.date());
            let range = (day(range.start()), day(range.end()));
            this.edit(cx, |f| {
                f.modified = Field {
                    on: true,
                    value: range,
                }
            });
        })
        .detach();
        panel
    }

    /// Edits `field` of the form whenever its text input changes.
    fn watch(
        &self,
        input: &Entity<InputState>,
        set: impl Fn(&mut Filters, String) + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe(input, move |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let value = input.read(cx).value().to_string();
                this.edit(cx, |f| set(f, value));
            }
        })
        .detach();
    }

    /// Applies `change` to the form and reports the Query text it now describes.
    fn edit(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Filters)) {
        let before = self.form.clone();
        change(&mut self.form);
        if self.form != before {
            cx.emit(self.form.write());
            cx.notify();
        }
    }

    /// Re-reads the form from Query `text`, e.g. when it was typed in the Query bar.
    /// Setting a widget's value does not report a change, so this never writes back.
    pub fn reload(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut form = Filters::read(text);
        // Fields the user unticked are not in the text; keep what they had typed.
        let old = &self.form;
        if !form.ext.on {
            form.ext = Field {
                on: false,
                ..old.ext.clone()
            };
        }
        if !form.regex.on {
            form.regex = Field {
                on: false,
                ..old.regex.clone()
            };
        }
        if !form.categories.on {
            form.categories = Field {
                on: false,
                ..old.categories.clone()
            };
        }
        if !form.size.on {
            form.size = Field {
                on: false,
                ..old.size.clone()
            };
        }
        if !form.modified.on {
            form.modified = Field {
                on: false,
                ..old.modified.clone()
            };
        }
        if form == self.form {
            return;
        }
        let mut set = |input: &Entity<InputState>, value: &str, cx: &mut Context<Self>| {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.name, &form.name.value, cx);
        set(&self.ext, &form.ext.value, cx);
        set(&self.regex, &form.regex.value, cx);
        set(&self.min, &form.size.value.0.amount, cx);
        set(&self.max, &form.size.value.1.amount, cx);
        let (from, to) = form.modified.value;
        self.dates
            .update(cx, |d, cx| d.set_date(Date::Range(from, to), window, cx));
        self.form = form;
        cx.notify();
    }

    /// How many fields other than Name add a term to the Query.
    pub fn active(&self) -> usize {
        self.form.active()
    }

    fn set_dates(&mut self, range: Dates, window: &mut Window, cx: &mut Context<Self>) {
        self.dates.update(cx, |d, cx| {
            d.set_date(Date::Range(range.0, range.1), window, cx)
        });
        self.edit(cx, |f| {
            f.modified = Field {
                on: true,
                value: range,
            };
        });
    }

    fn row(label: impl IntoElement, body: impl IntoElement) -> Div {
        h_flex()
            .gap_3()
            .items_center()
            .child(div().w(px(130.)).child(label))
            .child(body)
    }

    fn toggle(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        set: fn(&mut Filters, bool),
        cx: &mut Context<Self>,
    ) -> Checkbox {
        Checkbox::new(id)
            .label(label)
            .checked(on)
            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                let on = *on;
                this.edit(cx, |f| set(f, on));
            }))
    }

    fn categories(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .flex_wrap()
            .children(Category::ALL.map(|category| {
                let on = self.form.categories.value.contains(&category);
                let button = Button::new(("category", category as usize))
                    .label(category.name())
                    .small();
                let button = if on { button.primary() } else { button.ghost() };
                button.on_click(cx.listener(move |this, _, _, cx| {
                    this.edit(cx, |f| {
                        let list = &mut f.categories.value;
                        if on {
                            list.retain(|&c| c != category);
                        } else {
                            list.push(category);
                            // Keep the written order stable whatever order they were picked in.
                            list.sort_by_key(|c| Category::ALL.iter().position(|a| a == c));
                        }
                        f.categories.on = true;
                    });
                }))
            }))
    }

    fn bound(
        &self,
        label: &'static str,
        id: &'static str,
        input: &Entity<InputState>,
        bound: &Bound,
        max: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let danger = cx.theme().danger;
        h_flex()
            .gap_2()
            .items_center()
            .child(label)
            .child(div().w(px(90.)).child(Input::new(input)))
            .children(Unit::ALL.map(|unit| {
                let button = Button::new((id, unit as usize))
                    .label(unit.label())
                    .xsmall();
                let button = if bound.unit == unit {
                    button.primary()
                } else {
                    button.ghost()
                };
                button.on_click(cx.listener(move |this, _, _, cx| {
                    this.edit(cx, |f| {
                        let b = if max {
                            &mut f.size.value.1
                        } else {
                            &mut f.size.value.0
                        };
                        b.unit = unit;
                        f.size.on = true;
                    });
                }))
            }))
            .when(bound.invalid(), |row| {
                row.child(div().text_color(danger).text_sm().child("not a number"))
            })
    }

    fn dates(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let today = Local::now().date_naive();
        let presets: [(&'static str, Dates); 5] = [
            ("Today", (Some(today), None)),
            ("Last 7 days", (Some(today - Duration::days(6)), None)),
            ("Last 30 days", (Some(today - Duration::days(29)), None)),
            ("This year", (today.with_ordinal(1), None)),
            (
                "Older than a year",
                (
                    None,
                    today
                        .with_year(today.year() - 1)
                        .or(Some(today - Duration::days(365))),
                ),
            ),
        ];
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(presets.into_iter().enumerate().map(|(i, (label, range))| {
                        Button::new(("dm-preset", i))
                            .label(label)
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_dates(range, window, cx)
                            }))
                    })),
            )
            .child(
                div().w(px(320.)).child(
                    DatePicker::new(&self.dates)
                        .cleanable(true)
                        .placeholder("any date"),
                ),
            )
    }
}

impl Render for FiltersPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let f = self.form.clone();
        let muted = cx.theme().muted_foreground;
        let wide = |input: &Entity<InputState>| div().w(px(320.)).child(Input::new(input));
        v_flex()
            .gap_3()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .child(Self::row(
                self.toggle(
                    "on-name",
                    "Name contains",
                    f.name.on,
                    |f, on| f.name.on = on,
                    cx,
                ),
                v_flex()
                    .gap_1()
                    .child(wide(&self.name))
                    .when(f.name_has_query_syntax(), |c| {
                        c.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Some terms are only editable as text."),
                        )
                    }),
            ))
            .child(Self::row(
                self.toggle("on-ext", "Extensions", f.ext.on, |f, on| f.ext.on = on, cx),
                wide(&self.ext),
            ))
            .child(Self::row(
                self.toggle(
                    "on-type",
                    "Category",
                    f.categories.on,
                    |f, on| f.categories.on = on,
                    cx,
                ),
                self.categories(cx),
            ))
            .child(Self::row(
                self.toggle("on-size", "Size", f.size.on, |f, on| f.size.on = on, cx),
                v_flex()
                    .gap_1()
                    .child(self.bound(
                        "at least",
                        "min-unit",
                        &self.min,
                        &f.size.value.0,
                        false,
                        cx,
                    ))
                    .child(self.bound("at most", "max-unit", &self.max, &f.size.value.1, true, cx)),
            ))
            .child(Self::row(
                self.toggle(
                    "on-dm",
                    "Modified",
                    f.modified.on,
                    |f, on| f.modified.on = on,
                    cx,
                ),
                self.dates(cx),
            ))
            .child(Self::row(
                self.toggle("on-regex", "Regex", f.regex.on, |f, on| f.regex.on = on, cx),
                wide(&self.regex),
            ))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would glob gpui's `test` macro over `#[test]`.
    use super::{Bound, Field, Filters, Unit};
    use ve_core::Category;

    #[test]
    fn fields_round_trip_in_canonical_order() {
        let text = "regex:^a dm:2025-01-01..2025-02-01 size:>=1.5gb type:video|type:audio ext:mp4;mkv holiday";
        let form = Filters::read(text);
        assert_eq!(form.name.value, "holiday");
        assert_eq!(form.ext.value, "mp4;mkv");
        assert_eq!(
            form.categories.value,
            vec![Category::Video, Category::Audio]
        );
        assert_eq!(
            form.size.value.0,
            Bound {
                amount: "1.5".into(),
                unit: Unit::GB
            }
        );
        assert_eq!(form.active(), 5);
        assert_eq!(
            form.write(),
            "holiday ext:mp4;mkv type:video|type:audio size:>=1.5gb dm:2025-01-01..2025-02-01 regex:^a"
        );
    }

    #[test]
    fn open_ended_dates_round_trip() {
        for text in ["dm:>=2025-03-04", "dm:<=2025-03-04"] {
            assert_eq!(Filters::read(text).write(), text);
        }
    }

    #[test]
    fn clauses_the_form_cannot_show_stay_verbatim_in_name() {
        let text = r#"!*.tmp ext:mp4|type:video size:>1gb "a b" ext:jpg ext:png"#;
        let form = Filters::read(text);
        // A negation, a mixed OR, a strict comparison and the second ext: are left as typed.
        assert_eq!(
            form.name.value,
            r#"!*.tmp ext:mp4|type:video size:>1gb "a b" ext:png"#
        );
        assert_eq!(form.ext.value, "jpg");
        assert!(form.name_has_query_syntax());
        assert_eq!(
            form.write(),
            r#"!*.tmp ext:mp4|type:video size:>1gb "a b" ext:png ext:jpg"#
        );
    }

    #[test]
    fn unticked_or_empty_fields_write_nothing() {
        let mut form = Filters::read("ext:mp4 size:1kb..2kb");
        form.ext.on = false;
        form.size.value.1.amount.clear();
        assert_eq!(form.write(), "size:>=1kb");
        form.size.value.0.amount = "x".into();
        assert_eq!(form.write(), "");
        assert_eq!(form.active(), 0);
    }

    #[test]
    fn values_with_spaces_are_quoted_and_read_back() {
        let mut form = Filters::read("");
        form.regex = Field {
            on: true,
            value: "a b".into(),
        };
        let text = form.write();
        assert_eq!(text, r#"regex:"a b""#);
        assert_eq!(Filters::read(&text).regex.value, "a b");
    }
}
