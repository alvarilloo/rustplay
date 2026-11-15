// Keyboard access (Tab order, Enter/Space, arrow keys, Esc), the focus rectangle and the
// confirmation dialog for destructive actions.

use super::{App, ComboKind, Id, TITLE_H};
use crate::gfx::{Font, Rect};
use crate::i18n::{t, tf};
use super::widgets::Style;

const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfirmAction {
    ResetSettings,
    ForgetDevice(usize),
    ClearHistory,
    RunInstaller,
}

#[derive(Clone, Debug)]
pub struct Confirm {
    pub action: ConfirmAction,
    pub title: String,
    pub message: String,
    pub verb: String,
}

// Ids that only catch the pointer and never take keyboard focus.
fn focusable(id: Id) -> bool {
    !matches!(id, Id::ComboDismiss | Id::Modal | Id::Minimize | Id::Close)
}

impl App {
    // Called from `interact` with the unclipped rect, so controls scrolled out of view still
    // take part in the Tab order.
    pub(super) fn note_focusable(&mut self, rect: Rect, id: Id) {
        if focusable(id) {
            self.focusables.push((rect, id));
        }
    }

    pub(super) fn ask_confirm(&mut self, action: ConfirmAction) {
        let (title, message, verb) = match action {
            ConfirmAction::ResetSettings => (
                t("confirm.reset.title").to_owned(),
                t("confirm.reset.message").to_owned(),
                t("confirm.reset.action").to_owned(),
            ),
            ConfirmAction::ClearHistory => (
                t("confirm.history.title").to_owned(),
                t("confirm.history.message").to_owned(),
                t("confirm.history.action").to_owned(),
            ),
            ConfirmAction::RunInstaller => {
                let version = self.update_available().map(|r| r.version.clone()).unwrap_or_default();
                (
                    tf("confirm.install.title", &[("version", &version)]),
                    t("confirm.install.message").to_owned(),
                    t("confirm.install.action").to_owned(),
                )
            }
            ConfirmAction::ForgetDevice(i) => {
                let name = self
                    .settings
                    .known_devices
                    .get(i)
                    .map(|d| d.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| t("access.unknown_device").to_owned());
                (
                    tf("confirm.forget.title", &[("name", &name)]),
                    t("confirm.forget.message").to_owned(),
                    t("confirm.forget.action").to_owned(),
                )
            }
        };
        self.combo = None;
        self.anims.remove(&(Id::Modal, 1));
        self.confirm = Some(Confirm { action, title, message, verb });
        // Like ContentDialog with a destructive primary action, Cancel starts focused.
        self.kb_focus = Some(Id::ConfirmCancel);
    }

    pub(super) fn confirm_answer(&mut self, accepted: bool) {
        let Some(confirm) = self.confirm.take() else { return };
        self.kb_focus = None;
        if !accepted {
            return;
        }
        match confirm.action {
            ConfirmAction::ResetSettings => self.reset_settings(),
            ConfirmAction::ForgetDevice(i) => self.device_action(i, None),
            ConfirmAction::ClearHistory => self.history_clear(),
            ConfirmAction::RunInstaller => self.run_installer(),
        }
    }

    // Order of the controls drawn in the last frame; inside a dialog only its own controls.
    fn tab_order(&self) -> Vec<(Rect, Id)> {
        let modal = self.confirm.is_some() || self.pin.is_some() || self.setup_open() || self.welcome_open();
        let start = if modal { self.regions.iter().rposition(|(_, id)| *id == Id::Modal).map_or(0, |i| i + 1) } else { 0 };
        let modal_ids: Vec<Id> = self.regions[start..].iter().map(|(_, id)| *id).collect();
        let mut order: Vec<(Rect, Id)> = Vec::new();
        for &(rect, id) in &self.focusables {
            if modal && !modal_ids.contains(&id) {
                continue;
            }
            if matches!(id, Id::ComboItem(..)) || order.iter().any(|(_, seen)| *seen == id) {
                continue;
            }
            order.push((rect, id));
        }
        order
    }

    fn move_focus(&mut self, back: bool) {
        let order = self.tab_order();
        if order.is_empty() {
            return;
        }
        let current = self.kb_focus.and_then(|id| order.iter().position(|(_, o)| *o == id));
        let next = match (current, back) {
            (None, false) => 0,
            (None, true) => order.len() - 1,
            (Some(i), false) => (i + 1) % order.len(),
            (Some(i), true) => (i + order.len() - 1) % order.len(),
        };
        let (rect, id) = order[next];
        self.kb_focus = Some(id);
        self.focus_visible = true;
        self.reveal(rect);
    }

    // Scrolls the page so a focused control is fully visible.
    fn reveal(&mut self, rect: Rect) {
        if self.setup_open() && self.confirm.is_none() {
            self.setup_reveal(rect);
            return;
        }
        if self.confirm.is_some() || self.pin.is_some() || rect.x < super::NAV_W {
            return;
        }
        let (_, h) = self.gfx.size_dip();
        let top = TITLE_H + 16.0;
        let bottom = h - 16.0;
        if rect.bottom() > bottom {
            self.scroll_target = (self.scroll_target + rect.bottom() - bottom).min(self.scroll_max);
        } else if rect.y < top {
            self.scroll_target = (self.scroll_target - (top - rect.y)).max(0.0);
        }
    }

    // WM_KEYDOWN. Returns true when the key was used.
    pub fn key_down(&mut self, vk: u32, shift: bool) -> bool {
        if self.skip_splash() {
            return true;
        }
        // Typing in a text box: only Tab leaves it; everything else is text.
        if matches!(self.focus, Some(Id::NameBox | Id::AliasBox)) {
            if vk == VK_TAB {
                if self.focus == Some(Id::AliasBox) {
                    self.commit_alias();
                } else {
                    self.commit_name();
                }
                self.focus = None;
                self.move_focus(shift);
                return true;
            }
            return false;
        }
        if let Some(kind) = self.combo {
            return self.combo_key(kind, vk);
        }
        match vk {
            VK_TAB => {
                self.move_focus(shift);
                true
            }
            VK_ESCAPE if self.confirm.is_some() => {
                self.confirm_answer(false);
                true
            }
            VK_ESCAPE if self.welcome_open() && self.confirm.is_none() => {
                self.close_welcome();
                true
            }
            VK_RIGHT if self.welcome_open() && self.confirm.is_none() => {
                self.welcome_next();
                true
            }
            VK_LEFT if self.welcome_open() && self.confirm.is_none() => {
                self.welcome_back();
                true
            }
            VK_RETURN if self.welcome_open() && self.confirm.is_none() && !self.focus_visible => {
                self.welcome_next();
                true
            }
            VK_ESCAPE if self.setup_open() && self.pin.is_none() => {
                self.close_setup();
                true
            }
            VK_RETURN | VK_SPACE if self.pin.is_some() && vk == VK_RETURN => false,
            VK_RETURN | VK_SPACE => {
                let Some(id) = self.kb_focus.filter(|_| self.focus_visible) else { return false };
                self.activate(id);
                true
            }
            VK_LEFT | VK_RIGHT if self.focus_visible && self.kb_focus == Some(Id::RecvDelay) => {
                self.nudge_delay(if vk == VK_RIGHT { 1 } else { -1 });
                true
            }
            VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT if self.focus_visible && self.kb_focus.is_some() => {
                // Arrow keys walk the controls like Tab does, as in Windows Settings lists.
                self.move_focus(matches!(vk, VK_UP | VK_LEFT));
                true
            }
            _ => false,
        }
    }

    fn activate(&mut self, id: Id) {
        match id {
            Id::Combo(kind) => {
                self.click(id);
                if self.combo == Some(kind) {
                    self.combo_hot = Some(self.combo_selected(kind));
                }
            }
            Id::NameBox if self.receiver_editable() => {
                self.focus = Some(Id::NameBox);
                self.caret_epoch = std::time::Instant::now();
            }
            _ => self.click(id),
        }
    }

    fn combo_key(&mut self, kind: ComboKind, vk: u32) -> bool {
        let count = self.combo_items(kind).len();
        let hot = self.combo_hot.unwrap_or_else(|| self.combo_selected(kind).min(count.saturating_sub(1)));
        match vk {
            VK_UP => self.combo_hot = Some(hot.saturating_sub(1)),
            VK_DOWN => self.combo_hot = Some((hot + 1).min(count.saturating_sub(1))),
            VK_RETURN | VK_SPACE => {
                if count > 0 {
                    self.click(Id::ComboItem(kind, hot));
                }
                self.combo = None;
                self.combo_hot = None;
            }
            VK_ESCAPE | VK_TAB => {
                self.combo = None;
                self.combo_hot = None;
            }
            _ => return false,
        }
        // Keep the highlighted entry inside the scrolled part of a long list.
        if let Some(hot) = self.combo_hot {
            if hot < self.combo_scroll {
                self.combo_scroll = hot;
            } else if hot >= self.combo_scroll + self.combo_visible.max(1) {
                self.combo_scroll = hot + 1 - self.combo_visible.max(1);
            }
        }
        true
    }

    // Fluent focus visual: 2 px outer stroke and 1 px inner stroke around the control.
    pub(super) fn draw_focus(&self) {
        if !self.focus_visible || self.combo.is_some() {
            return;
        }
        let Some(id) = self.kb_focus else { return };
        let Some((rect, _)) = self.focusables.iter().rev().find(|(_, o)| *o == id) else { return };
        let th = self.theme;
        let outer = rect.inset(-3.0, -3.0);
        self.gfx.stroke(outer, 7.0, 2.0, th.focus_outer);
        self.gfx.stroke(rect.inset(-1.0, -1.0), 5.0, 1.0, th.focus_inner);
    }

    // ContentDialog for destructive actions: title, message, the verb and Cancel.
    pub(super) fn draw_confirm(&mut self, w: f32, h: f32) {
        let Some(confirm) = self.confirm.clone() else { return };
        let th = self.theme;
        let fade = self.anim((Id::Modal, 1), 1.0, 16.0);
        self.region(Rect::new(0.0, 0.0, w, h), Id::Modal);
        self.gfx.fill(Rect::new(0.0, 0.0, w, h), 0.0, th.smoke.alpha(fade));
        let dw = 440.0;
        let (_, mh) = self.gfx.measure(&confirm.message, Font::Body, dw - 48.0, true);
        let (_, th_h) = self.gfx.measure(&confirm.title, Font::Subtitle, dw - 48.0, true);
        let body_h = 24.0 + th_h + 12.0 + mh + 24.0;
        let dh = body_h + 80.0;
        let rect = Rect::new((w - dw) / 2.0, (h - dh) / 2.0 + 16.0 * (1.0 - fade), dw, dh);
        self.gfx.push_layer(fade, 0.0);
        self.shadow(rect, 8.0, 32.0);
        self.gfx.fill(rect, 8.0, th.dialog);
        self.gfx.fill(Rect::new(rect.x, rect.bottom() - 80.0, dw, 80.0), 8.0, th.dialog_footer);
        self.gfx.fill(Rect::new(rect.x, rect.bottom() - 80.0, dw, 12.0), 0.0, th.dialog_footer);
        self.gfx.line(rect.x, rect.bottom() - 80.0, rect.right(), rect.bottom() - 80.0, 1.0, th.card_stroke);
        self.gfx.stroke(rect, 8.0, 1.0, th.flyout_stroke);
        let x = rect.x + 24.0;
        let y = rect.y + 24.0;
        self.gfx.paragraph(&confirm.title, Font::Subtitle, Rect::new(x, y, dw - 48.0, th_h + 4.0), th.text);
        self.gfx.paragraph(&confirm.message, Font::Body, Rect::new(x, y + th_h + 12.0, dw - 48.0, mh + 4.0), th.text);
        let fy = rect.bottom() - 80.0 + 24.0;
        let bw = (dw - 48.0 - 8.0) / 2.0;
        self.button(Id::ConfirmOk, Rect::new(x, fy, bw, 32.0), &confirm.verb, None, Style::Accent, true);
        self.button(Id::ConfirmCancel, Rect::new(x + bw + 8.0, fy, bw, 32.0), t("common.cancel"), None, Style::Standard, true);
        self.gfx.pop_layer();
    }
}
