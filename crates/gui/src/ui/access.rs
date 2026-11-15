// Access control of the receiver: PIN prompts, "ask every time" prompts and the known devices.

use std::net::SocketAddr;

use rustplay_rx::{AccessConfig, AccessPolicy, PinConfig};

use super::widgets::Style;
use super::{App, ComboKind, GAP, Id, ROW_H, glyph};
use crate::backend::Command;
use crate::gfx::{Align, Color, Font, Rect};
use crate::i18n::{t, tf};
use crate::popup::{self, AskButtons, Palette, Rgb, Spec};
use crate::settings::{DeviceState, KnownDevice};

const PIN_POPUP_SECS: u32 = 180;
const ASK_POPUP_SECS: u32 = 35;
const CARD_H: f32 = 60.0;

pub(super) enum PopupKind {
    Ask { id: u64, device_id: String },
    Pin { peer: SocketAddr },
}

pub(super) struct PopupEntry {
    pub tag: u64,
    pub handle: isize,
    pub kind: PopupKind,
}

// `color` blended over the opaque `over`.
fn flat(color: Color, over: Color) -> Rgb {
    let channel = |c: f32, b: f32| ((c * color.a + b * (1.0 - color.a)) * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgb(channel(color.r, over.r), channel(color.g, over.g), channel(color.b, over.b))
}

impl App {
    pub(super) fn access_config(&self) -> AccessConfig {
        AccessConfig {
            policy: match self.settings.access_policy {
                1 => AccessPolicy::AllowedOnly,
                2 => AccessPolicy::Ask,
                _ => AccessPolicy::Everyone,
            },
            allowed: self.settings.devices_in(DeviceState::Allowed),
            blocked: self.settings.devices_in(DeviceState::Blocked),
        }
    }

    pub(super) fn pin_config(&self) -> PinConfig {
        PinConfig { required: self.settings.receiver_pin, fixed: None }
    }

    fn palette(&self) -> Palette {
        let th = self.theme;
        let bg = th.dialog;
        Palette {
            background: flat(bg, bg),
            border: flat(th.text.alpha(0.14), bg),
            text: flat(th.text, bg),
            text_secondary: flat(th.text_secondary, bg),
            button: flat(th.text.alpha(0.06), bg),
            button_hover: flat(th.text.alpha(0.10), bg),
            accent: flat(th.accent, bg),
            accent_hover: flat(th.accent.alpha(0.88), bg),
            text_on_accent: flat(th.text_on_accent, bg),
        }
    }

    fn open_popup(&mut self, tag: u64, kind: PopupKind, mut spec: Spec) {
        spec.slot = self.popups.len() as u32;
        if let Some(handle) = popup::show(self.hwnd, tag, spec) {
            self.popups.push(PopupEntry { tag, handle, kind });
        }
    }

    fn close_popups(&mut self, keep: impl Fn(&PopupKind) -> bool) {
        self.popups.retain(|p| {
            let keep = keep(&p.kind);
            if !keep {
                popup::close(p.handle);
            }
            keep
        });
    }

    pub(super) fn close_all_popups(&mut self) {
        self.close_popups(|_| false);
        self.rx_pin = None;
    }

    fn device_label(&self, device_id: &str, name: Option<&str>) -> String {
        name.filter(|n| !n.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                self.settings
                    .known_devices
                    .iter()
                    .find(|d| d.id == device_id && !d.name.is_empty())
                    .map(|d| d.name.clone())
            })
            .unwrap_or_else(|| t("access.unknown_device").to_owned())
    }

    pub(super) fn note_device(&mut self, device_id: &str, name: Option<&str>, model: Option<&str>) {
        let name = name.map(str::trim).unwrap_or_default();
        let model = super::friendly_model(model.unwrap_or_default());
        match self.settings.known_devices.iter_mut().find(|d| d.id == device_id) {
            Some(known) => {
                let mut changed = false;
                if !name.is_empty() && known.name != name {
                    known.name = name.to_owned();
                    changed = true;
                }
                if !model.is_empty() && known.model != model {
                    known.model = model;
                    changed = true;
                }
                if !changed {
                    return;
                }
            }
            None => self.settings.known_devices.push(KnownDevice {
                id: device_id.to_owned(),
                name: name.to_owned(),
                model,
                state: DeviceState::Known,
            }),
        }
        self.settings.save();
    }

    pub(super) fn access_request(&mut self, id: u64, peer: SocketAddr, device_id: &str, name: Option<&str>) {
        let label = self.device_label(device_id, name);
        let ip = crate::privacy::redact(&peer.ip().to_string()).into_owned();
        let spec = Spec {
            title: tf("access.ask.title", &[("name", &label)]),
            body: tf("access.ask.body", &[("address", &ip)]),
            code: None,
            buttons: Some(AskButtons {
                allow: t("access.ask.allow").to_owned(),
                deny: t("access.ask.deny").to_owned(),
                remember: t("access.ask.remember").to_owned(),
            }),
            palette: self.palette(),
            expire_secs: ASK_POPUP_SECS,
            slot: 0,
        };
        self.open_popup(id, PopupKind::Ask { id, device_id: device_id.to_owned() }, spec);
    }

    pub(super) fn access_resolved(&mut self, id: u64) {
        self.close_popups(|k| !matches!(k, PopupKind::Ask { id: i, .. } if *i == id));
    }

    // Popups follow the device's address, not the connection: iOS asks for the code on one
    // connection and sends it on the next.
    pub(super) fn pin_shown(&mut self, peer: SocketAddr, pin: String) {
        if self.rx_pin.as_ref().is_some_and(|(p, code)| p.ip() == peer.ip() && *code == pin) {
            return;
        }
        self.close_popups(|k| !matches!(k, PopupKind::Pin { peer: p } if p.ip() == peer.ip()));
        let spec = Spec {
            title: t("access.pin.title").to_owned(),
            body: t("access.pin.body").to_owned(),
            code: Some(pin.clone()),
            buttons: None,
            palette: self.palette(),
            expire_secs: PIN_POPUP_SECS,
            slot: 0,
        };
        self.next_popup_tag += 1;
        let tag = u64::MAX - self.next_popup_tag;
        self.open_popup(tag, PopupKind::Pin { peer }, spec);
        self.rx_pin = Some((peer, pin));
    }

    pub(super) fn pin_finished(&mut self, peer: SocketAddr) {
        self.close_popups(|k| !matches!(k, PopupKind::Pin { peer: p } if p.ip() == peer.ip()));
        if self.rx_pin.as_ref().is_some_and(|(p, _)| p.ip() == peer.ip()) {
            self.rx_pin = None;
        }
    }

    // Answers of the prompts, delivered by `popup::WM_POPUP`.
    pub fn on_popups(&mut self) {
        for (tag, outcome) in popup::take_results() {
            let Some(pos) = self.popups.iter().position(|p| p.tag == tag) else { continue };
            let entry = self.popups.remove(pos);
            let PopupKind::Ask { id, device_id } = entry.kind else { continue };
            self.send(Command::AnswerAccess { id, allow: outcome.allow });
            if outcome.remember {
                let state = if outcome.allow { DeviceState::Allowed } else { DeviceState::Blocked };
                self.set_device_state(&device_id, state);
            }
        }
    }

    fn set_device_state(&mut self, device_id: &str, state: DeviceState) {
        match self.settings.known_devices.iter_mut().find(|d| d.id == device_id) {
            Some(device) => device.state = state,
            None => self.settings.known_devices.push(KnownDevice {
                id: device_id.to_owned(),
                name: String::new(),
                model: String::new(),
                state,
            }),
        }
        self.settings.save();
        self.update_receiver();
    }

    // `state: None` forgets the device.
    pub(super) fn device_action(&mut self, index: usize, state: Option<DeviceState>) {
        let Some(device) = self.settings.known_devices.get(index) else { return };
        let id = device.id.clone();
        match state {
            Some(state) => self.set_device_state(&id, state),
            None => {
                self.settings.known_devices.remove(index);
                if let Err(e) = rustplay_rx::forget_paired_device(&id) {
                    tracing::warn!("no se pudo olvidar el emparejamiento de {id}: {e}");
                }
                self.settings.save();
                self.update_receiver();
            }
        }
    }

    pub(super) fn pin_hero(&self) -> Option<(String, String)> {
        let (_, pin) = self.rx_pin.as_ref()?;
        let spaced = pin.chars().map(String::from).collect::<Vec<_>>().join(" ");
        Some((t("receive.hero.pin").to_owned(), tf("receive.hero.pin_body", &[("code", &spaced)])))
    }

    // Access rows and the known devices of the Receive page; returns the new y.
    pub(super) fn draw_access(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let th = self.theme;
        let editable = self.receiver_editable();
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::LOCK, t("receive.access.pin"), t("receive.access.pin_desc"), 160.0, editable);
        self.toggle_with_label(Id::RecvPin, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.receiver_pin, editable);
        y += ROW_H + GAP;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::PEOPLE, t("receive.access.who"), t("receive.access.who_desc"), 260.0, true);
        let label = self.combo_label(ComboKind::RecvAccess);
        self.combo_box(ComboKind::RecvAccess, Rect::new(rect.right() - 16.0 - 260.0, rect.y + 18.0, 260.0, 32.0), &label, true);
        y += ROW_H;
        if self.settings.known_devices.is_empty() {
            return y;
        }
        y += 28.0;
        self.section(x, y, w, t("receive.access.devices"));
        y += 30.0;
        let labels = [t("receive.access.allow"), t("receive.access.block"), t("receive.access.forget")];
        let widths = labels.map(|label| self.button_width(label, None));
        let total: f32 = widths.iter().sum::<f32>() + 8.0 * 2.0;
        for index in 0..self.settings.known_devices.len() {
            let device = self.settings.known_devices[index].clone();
            let rect = Rect::new(x, y, w, CARD_H);
            self.card(rect);
            let cy = rect.y + rect.h / 2.0;
            let tile = Rect::new(rect.x + 16.0, cy - 18.0, 36.0, 36.0);
            self.gfx.fill(tile, 8.0, th.accent.alpha(0.16));
            self.gfx.glyph(glyph::PHONE, Font::Icon20, tile.x + 18.0, tile.y + 18.0, th.text);
            let name = if device.name.is_empty() { t("access.unknown_device") } else { device.name.as_str() };
            let id_text = if crate::privacy::hide_ips() { "" } else { device.id.as_str() };
            let detail = [device.model.as_str(), id_text].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ·  ");
            let text_w = (w - 66.0 - total - 200.0).max(120.0);
            self.gfx.text(name, Font::Body, Rect::new(rect.x + 66.0, cy - 19.0, text_w, 20.0), th.text, Align::Start);
            self.gfx.text(&detail, Font::Caption, Rect::new(rect.x + 66.0, cy + 1.0, text_w, 18.0), th.text_secondary, Align::Start);
            let (state_text, state_color) = match device.state {
                DeviceState::Allowed => (t("receive.access.state.allowed"), th.success),
                DeviceState::Blocked => (t("receive.access.state.blocked"), th.critical),
                DeviceState::Known => (t("receive.access.state.known"), th.text_tertiary),
            };
            let mut bx = rect.right() - 16.0 - total;
            let badge_w = self.badge_width(state_text);
            self.badge(bx - 12.0 - badge_w, cy, state_text, state_color);
            for (which, (label, width)) in labels.iter().zip(widths).enumerate() {
                let enabled = match which {
                    0 => device.state != DeviceState::Allowed,
                    1 => device.state != DeviceState::Blocked,
                    _ => true,
                };
                let id = match which {
                    0 => Id::DeviceAllow(index),
                    1 => Id::DeviceBlock(index),
                    _ => Id::DeviceForget(index),
                };
                self.button(id, Rect::new(bx, cy - 16.0, width, 32.0), label, None, Style::Standard, enabled);
                bx += width + 8.0;
            }
            y += CARD_H + GAP;
        }
        y - GAP
    }
}
