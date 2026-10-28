//! The Filters dialog: a form over the Query text. Opening it reads the clauses it can show as
//! fields; every edit writes the whole text back in a fixed order, and clauses the form cannot
//! show (`|` across modifiers, `!`, repeats, other comparisons) stay verbatim in Name contains.

use egui::{ComboBox, RichText, Ui};
use egui_extras::DatePickerButton;
use jiff::civil::Date;
use jiff::{ToSpan, Zoned};
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
        Some(format!("{amount}{}", self.unit.label().to_ascii_lowercase()))
    }
}

/// A field with the checkbox that decides whether it is written.
#[derive(Clone, PartialEq, Debug)]
pub struct Field<T> {
    pub on: bool,
    pub value: T,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Filters {
    /// Plain name text, plus every clause the other fields cannot show, as typed.
    pub name: Field<String>,
    /// Extensions separated by `;`.
    pub ext: Field<String>,
    pub categories: Field<Vec<Category>>,
    pub size: Field<(Bound, Bound)>,
    pub modified: Field<(Option<Date>, Option<Date>)>,
    pub regex: Field<String>,
}

impl Filters {
    /// The form for Query `text`.
    pub fn read(text: &str) -> Filters {
        let mut form = Filters {
            name: Field { on: true, value: String::new() },
            ext: Field { on: false, value: String::new() },
            categories: Field { on: false, value: Vec::new() },
            size: Field { on: false, value: (Bound::empty(), Bound::empty()) },
            modified: Field { on: false, value: (None, None) },
            regex: Field { on: false, value: String::new() },
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
            self.ext = Field { on: true, value: value.clone() };
        } else if single("regex") && !self.regex.on {
            self.regex = Field { on: true, value: value.clone() };
        } else if single("size") && !self.size.on {
            let Some(range) = size_range(value) else {
                return false;
            };
            self.size = Field { on: true, value: range };
        } else if single("dm") && !self.modified.on {
            let Some(range) = date_range(value) else {
                return false;
            };
            self.modified = Field { on: true, value: range };
        } else if modifier("type") && !self.categories.on {
            let parsed: Option<Vec<Category>> = clause
                .words
                .iter()
                .map(|w: &WordSource| Category::parse(&w.value))
                .collect();
            let Some(categories) = parsed else {
                return false;
            };
            self.categories = Field { on: true, value: categories };
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
        clauses(&self.name.value).iter().any(|c| {
            c.words.len() > 1 || c.words.iter().any(|w| w.negated || w.modifier.is_some())
        })
    }

    /// Draws the form; returns true when a field changed.
    pub fn ui(&mut self, ui: &mut Ui) -> bool {
        let before = self.clone();
        let muted = crate::theme::palette(ui).muted;
        egui::Grid::new("filters")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.checkbox(&mut self.name.on, "Name contains");
                ui.add_enabled_ui(self.name.on, |ui| {
                    ui.vertical(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.name.value)
                                .hint_text("part of a name")
                                .desired_width(320.0),
                        );
                        if self.name_has_query_syntax() {
                            ui.label(
                                RichText::new("Some terms are only editable as text.")
                                    .small()
                                    .color(muted),
                            );
                        }
                    });
                });
                ui.end_row();

                ui.checkbox(&mut self.ext.on, "Extensions");
                ui.add_enabled_ui(self.ext.on, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ext.value)
                            .hint_text("mp4;mkv")
                            .desired_width(320.0),
                    );
                });
                ui.end_row();

                ui.checkbox(&mut self.categories.on, "Category");
                ui.add_enabled_ui(self.categories.on, |ui| category_picker(ui, &mut self.categories.value));
                ui.end_row();

                ui.checkbox(&mut self.size.on, "Size");
                ui.add_enabled_ui(self.size.on, |ui| {
                    ui.horizontal(|ui| {
                        let (min, max) = &mut self.size.value;
                        bound_editor(ui, "at least", "size-min", min);
                        bound_editor(ui, "at most", "size-max", max);
                    });
                });
                ui.end_row();

                ui.checkbox(&mut self.modified.on, "Modified");
                ui.add_enabled_ui(self.modified.on, |ui| date_editor(ui, &mut self.modified.value));
                ui.end_row();

                ui.checkbox(&mut self.regex.on, "Regex");
                ui.add_enabled_ui(self.regex.on, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.regex.value)
                            .hint_text("matches the name")
                            .font(egui::TextStyle::Monospace)
                            .desired_width(320.0),
                    );
                });
                ui.end_row();
            });
        *self != before
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
fn date_range(value: &str) -> Option<(Option<Date>, Option<Date>)> {
    let date = |text: &str| -> Option<Date> {
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

fn category_picker(ui: &mut Ui, selected: &mut Vec<Category>) {
    let summary = if selected.is_empty() {
        "Any".to_owned()
    } else {
        let names: Vec<&str> = selected.iter().map(|c| c.name()).collect();
        names.join(", ")
    };
    ComboBox::from_id_salt("categories")
        .selected_text(summary)
        .width(320.0)
        .show_ui(ui, |ui| {
            for category in Category::ALL {
                let mut on = selected.contains(&category);
                let swatch = RichText::new("■ ").color(crate::treemap::category_color(category));
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            selected.push(category);
                        } else {
                            selected.retain(|&c| c != category);
                        }
                    }
                    ui.label(swatch);
                    ui.label(category.name());
                });
            }
        });
    // Keep the written order stable whatever order the boxes were ticked in.
    selected.sort_by_key(|c| Category::ALL.iter().position(|a| a == c));
}

fn bound_editor(ui: &mut Ui, label: &str, id: &str, bound: &mut Bound) {
    ui.label(label);
    let valid = bound.amount.trim().is_empty() || bound.amount.trim().parse::<f64>().is_ok();
    let mut edit = egui::TextEdit::singleline(&mut bound.amount)
        .hint_text("—")
        .font(egui::TextStyle::Monospace)
        .desired_width(64.0);
    if !valid {
        edit = edit.text_color(ui.visuals().error_fg_color);
    }
    ui.add(edit);
    ComboBox::from_id_salt(id)
        .selected_text(bound.unit.label())
        .width(56.0)
        .show_ui(ui, |ui| {
            for unit in Unit::ALL {
                ui.selectable_value(&mut bound.unit, unit, unit.label());
            }
        });
}

fn date_editor(ui: &mut Ui, (from, to): &mut (Option<Date>, Option<Date>)) {
    let today = Zoned::now().date();
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            for (label, range) in [
                ("Today", (Some(today), None)),
                ("Last 7 days", (Some(today - 6.days()), None)),
                ("Last 30 days", (Some(today - 29.days()), None)),
                ("This year", (Some(today.first_of_year()), None)),
                ("Older than a year", (None, Some(today - 1.year()))),
            ] {
                if ui.small_button(label).clicked() {
                    (*from, *to) = range;
                }
            }
        });
        ui.horizontal(|ui| {
            date_bound(ui, "from", "dm-from", from, today);
            date_bound(ui, "to", "dm-to", to, today);
        });
    });
}

/// An optional date: a checkbox, then the picker once ticked.
fn date_bound(ui: &mut Ui, label: &str, id: &str, date: &mut Option<Date>, today: Date) {
    let mut on = date.is_some();
    if ui.checkbox(&mut on, label).changed() {
        *date = on.then_some(today);
    }
    match date {
        Some(d) => {
            ui.add(DatePickerButton::new(d).id_salt(id).calendar_week(false));
        }
        None => {
            ui.label(RichText::new("any").color(crate::theme::palette(ui).muted));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip_in_canonical_order() {
        let text = "regex:^a dm:2025-01-01..2025-02-01 size:>=1.5gb type:video|type:audio ext:mp4;mkv holiday";
        let form = Filters::read(text);
        assert_eq!(form.name.value, "holiday");
        assert_eq!(form.ext.value, "mp4;mkv");
        assert_eq!(form.categories.value, vec![Category::Video, Category::Audio]);
        assert_eq!(form.size.value.0, Bound { amount: "1.5".into(), unit: Unit::GB });
        assert_eq!(form.active(), 5);
        assert_eq!(
            form.write(),
            "holiday ext:mp4;mkv type:video|type:audio size:>=1.5gb dm:2025-01-01..2025-02-01 regex:^a"
        );
    }

    #[test]
    fn clauses_the_form_cannot_show_stay_verbatim_in_name() {
        let text = r#"!*.tmp ext:mp4|type:video size:>1gb "a b" ext:jpg ext:png"#;
        let form = Filters::read(text);
        // A negation, a mixed OR, a strict comparison and the second ext: are left as typed.
        assert_eq!(form.name.value, r#"!*.tmp ext:mp4|type:video size:>1gb "a b" ext:png"#);
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
        form.regex = Field { on: true, value: "a b".into() };
        let text = form.write();
        assert_eq!(text, r#"regex:"a b""#);
        assert_eq!(Filters::read(&text).regex.value, "a b");
    }
}
