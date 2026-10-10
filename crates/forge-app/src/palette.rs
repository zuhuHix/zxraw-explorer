//! Command palette (Ctrl+Shift+P): fuzzy-filter every command by name.

use egui::{Key, RichText};

use crate::commands::Cmd;

#[derive(Default)]
pub struct Palette {
    pub query: String,
    pub selected: usize,
    pub open: bool,
    focused_once: bool,
}

/// Subsequence fuzzy score; higher is better, `None` = no match.
pub fn fuzzy_score(hay: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = hay.to_lowercase().chars().collect();
    let mut score = 0;
    let mut hi = 0;
    let mut prev_match = false;
    for nc in needle.to_lowercase().chars() {
        let mut found = false;
        while hi < h.len() {
            let c = h[hi];
            hi += 1;
            if c == nc {
                score += if prev_match { 5 } else { 1 };
                if hi == 1 || h.get(hi.wrapping_sub(2)).is_some_and(|p| *p == ' ') {
                    score += 8; // word start
                }
                prev_match = true;
                found = true;
                break;
            }
            prev_match = false;
        }
        if !found {
            return None;
        }
    }
    Some(score - h.len() as i32 / 8)
}

impl Palette {
    /// Returns the chosen command, if any. Sets `open = false` when dismissed.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Cmd> {
        if !self.focused_once {
            self.open = true;
        }
        let mut items: Vec<(i32, Cmd)> = Cmd::ALL
            .iter()
            .filter(|c| **c != Cmd::CommandPalette)
            .filter_map(|c| fuzzy_score(c.label(), &self.query).map(|s| (s, *c)))
            .collect();
        items.sort_by(|a, b| b.0.cmp(&a.0));
        self.selected = self.selected.min(items.len().saturating_sub(1));

        let mut chosen = None;
        let (up, down, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::Escape),
            )
        });
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        if down && self.selected + 1 < items.len() {
            self.selected += 1;
        }
        if enter {
            chosen = items.get(self.selected).map(|x| x.1);
        }
        if esc {
            self.open = false;
        }

        let modal = egui::Modal::new(egui::Id::new("palette")).show(ctx, |ui| {
            ui.set_width(520.0);
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Type a command…")
                    .desired_width(f32::INFINITY),
            );
            if !self.focused_once {
                r.request_focus();
                self.focused_once = true;
            }
            if r.changed() {
                self.selected = 0;
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (i, (_, c)) in items.iter().enumerate() {
                    let sel = i == self.selected;
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::Button::selectable(sel, c.label())
                                .min_size(egui::vec2(380.0, 0.0)),
                        );
                        if sel && (up || down) {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            chosen = Some(*c);
                        }
                        ui.label(RichText::new(c.shortcut_text(ui.ctx())).weak().small());
                    });
                }
            });
        });
        if modal.should_close() {
            self.open = false;
        }
        if chosen.is_some() {
            self.open = false;
        }
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("Find duplicates…", "dup").is_some());
        assert!(fuzzy_score("Find duplicates…", "fd").is_some());
        assert!(fuzzy_score("Refresh", "xyz").is_none());
        assert!(fuzzy_score("New tab", "nt").unwrap() > fuzzy_score("Next tab", "nt").unwrap_or(-100) - 10);
    }
}
