//! Screenreader-Unterstützung über UI Automation.
//!
//! Die Leiste zeichnet alles selbst; ohne Provider sähe ein Screenreader nur
//! ein leeres Fenster. Hier wird sie als Symbolleiste mit Schaltflächen
//! beschrieben (Start, Apps, Tray-Icons, Status, Uhr …). Jede Schaltfläche hat
//! einen vorlesbaren Namen, ihre Bildschirmposition und lässt sich über das
//! Invoke-Muster auslösen. Die Elementliste liefert die Leiste selbst
//! (`bar::ui_elements`), damit Layout und Barrierefreiheit nie auseinanderlaufen.

// Konstanten der Windows-API (UIA_NamePropertyId, NavigateDirection_Parent …)
// behalten ihre Originalnamen.
#![allow(non_upper_case_globals)]

use windows::core::{implement, Error, IUnknown, Interface, Result, VARIANT};
use windows::Win32::Foundation::{HWND, S_OK};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
use windows::Win32::System::Variant::VT_I4;
use windows::Win32::UI::Accessibility::*;

/// Element gibt es nicht (mehr) – z. B. weil sich die Leiste geändert hat.
const UIA_E_ELEMENTNOTAVAILABLE: i32 = 0x8004_0201_u32 as i32;

/// "Kein Element" für UIA: S_OK mit leerem Ergebnis.
fn none<T>() -> Result<T> {
    Err(Error::new(S_OK, ""))
}

fn gone<T>() -> Result<T> {
    Err(Error::new(windows::core::HRESULT(UIA_E_ELEMENTNOTAVAILABLE), "Element nicht mehr vorhanden"))
}

fn root_fragment(hwnd: isize) -> IRawElementProviderFragment {
    Root { hwnd }.into()
}

fn item_fragment(hwnd: isize, index: usize) -> Result<IRawElementProviderFragment> {
    if index < crate::bar::ui_elements().len() {
        Ok(Item { hwnd, index }.into())
    } else {
        none()
    }
}

/// Provider der Leiste für WM_GETOBJECT.
pub fn root(hwnd: isize) -> IRawElementProviderSimple {
    Root { hwnd }.into()
}

/// Provider eines Elements (für Fokus-Ereignisse).
pub fn item(hwnd: isize, index: usize) -> IRawElementProviderSimple {
    Item { hwnd, index }.into()
}

/// Fokuswechsel melden – nur, wenn überhaupt ein Screenreader zuhört.
pub fn raise_focus(hwnd: isize, index: usize) {
    unsafe {
        if UiaClientsAreListening().as_bool() {
            let _ = UiaRaiseAutomationEvent(&item(hwnd, index), UIA_AutomationFocusChangedEventId);
        }
    }
}

// ----------------------------------------------------------------- Wurzel

#[implement(IRawElementProviderSimple, IRawElementProviderFragment, IRawElementProviderFragmentRoot)]
struct Root {
    hwnd: isize,
}

impl IRawElementProviderSimple_Impl for Root_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider | ProviderOptions_UseComThreading)
    }
    fn GetPatternProvider(&self, _: UIA_PATTERN_ID) -> Result<IUnknown> {
        none()
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        Ok(match id {
            UIA_NamePropertyId => VARIANT::from(t!("Taskleiste", "Taskbar")),
            UIA_ControlTypePropertyId => VARIANT::from(UIA_ToolBarControlTypeId.0),
            UIA_AutomationIdPropertyId => VARIANT::from("Lightbar"),
            _ => VARIANT::default(),
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        unsafe { UiaHostProviderFromHwnd(HWND(self.hwnd as _)) }
    }
}

impl IRawElementProviderFragment_Impl for Root_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let n = crate::bar::ui_elements().len();
        match direction {
            NavigateDirection_FirstChild if n > 0 => item_fragment(self.hwnd, 0),
            NavigateDirection_LastChild if n > 0 => item_fragment(self.hwnd, n - 1),
            _ => none(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // Die Wurzel bekommt ihre Identität vom Host-Fenster.
        Ok(std::ptr::null_mut())
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        Ok(UiaRect::default())
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(Root { hwnd: self.hwnd }.into())
    }
}

impl IRawElementProviderFragmentRoot_Impl for Root_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let (x, y) = (x as i32, y as i32);
        match crate::bar::ui_elements().iter().position(|e| x >= e.rect.left && x < e.rect.right && y >= e.rect.top && y < e.rect.bottom) {
            Some(i) => item_fragment(self.hwnd, i),
            None => none(),
        }
    }
    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        match crate::bar::ui_focus() {
            Some(i) => item_fragment(self.hwnd, i),
            None => none(),
        }
    }
}

// ------------------------------------------------------------ Elemente

#[implement(IRawElementProviderSimple, IRawElementProviderFragment, IInvokeProvider)]
struct Item {
    hwnd: isize,
    index: usize,
}

impl Item_Impl {
    fn element(&self) -> Result<crate::bar::UiElement> {
        match crate::bar::ui_elements().into_iter().nth(self.index) {
            Some(e) => Ok(e),
            None => gone(),
        }
    }
}

impl IRawElementProviderSimple_Impl for Item_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider | ProviderOptions_UseComThreading)
    }
    fn GetPatternProvider(&self, pattern: UIA_PATTERN_ID) -> Result<IUnknown> {
        if pattern == UIA_InvokePatternId {
            let invoke: IInvokeProvider = Item { hwnd: self.hwnd, index: self.index }.into();
            invoke.cast()
        } else {
            none()
        }
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let e = self.element()?;
        Ok(match id {
            UIA_NamePropertyId => VARIANT::from(e.name.as_str()),
            UIA_HelpTextPropertyId => VARIANT::from(e.help.as_str()),
            UIA_ControlTypePropertyId => VARIANT::from(UIA_ButtonControlTypeId.0),
            UIA_AutomationIdPropertyId => VARIANT::from(e.id.as_str()),
            UIA_IsKeyboardFocusablePropertyId | UIA_IsEnabledPropertyId | UIA_IsInvokePatternAvailablePropertyId => VARIANT::from(true),
            UIA_HasKeyboardFocusPropertyId => VARIANT::from(crate::bar::ui_focus() == Some(self.index)),
            _ => VARIANT::default(),
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        none()
    }
}

impl IRawElementProviderFragment_Impl for Item_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        match direction {
            NavigateDirection_Parent => Ok(root_fragment(self.hwnd)),
            NavigateDirection_NextSibling => item_fragment(self.hwnd, self.index + 1),
            NavigateDirection_PreviousSibling if self.index > 0 => item_fragment(self.hwnd, self.index - 1),
            _ => none(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        unsafe {
            let sa = SafeArrayCreateVector(VT_I4, 0, 2);
            for (i, v) in [UiaAppendRuntimeId as i32, self.index as i32 + 1].iter().enumerate() {
                SafeArrayPutElement(sa, &(i as i32), v as *const i32 as *const _)?;
            }
            Ok(sa)
        }
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let r = self.element()?.rect;
        Ok(UiaRect { left: r.left as f64, top: r.top as f64, width: (r.right - r.left) as f64, height: (r.bottom - r.top) as f64 })
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        crate::bar::ui_set_focus(self.index);
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(Root { hwnd: self.hwnd }.into())
    }
}

impl IInvokeProvider_Impl for Item_Impl {
    fn Invoke(&self) -> Result<()> {
        self.element()?;
        crate::bar::ui_invoke(self.index);
        Ok(())
    }
}

// ======================================================= Startmenü
//
// Gleiches Muster für das Startmenü: die Wurzel ist das Fenster „Start“,
// darunter Suchfeld (Edit mit Value-Muster), Kacheln und Listenzeilen
// (ListItem) und Schaltflächen. Die Elemente liefert `start::ui_nodes` aus dem
// zuletzt gezeichneten Frame; ein Element ist über seinen Fokus-Code
// (`start::Focus::code`) bestimmt, nicht über seine Position – so bleibt es
// dasselbe, auch wenn sich die Liste beim Scrollen verschiebt.

/// Provider des Startmenüs für WM_GETOBJECT.
pub fn start_root(hwnd: isize) -> IRawElementProviderSimple {
    StartRoot { hwnd }.into()
}

/// Fokuswechsel im Startmenü melden – nur, wenn ein Screenreader zuhört.
pub fn start_focus_changed(hwnd: isize, code: usize) {
    unsafe {
        if UiaClientsAreListening().as_bool() {
            let el: IRawElementProviderSimple = StartItem { hwnd, code }.into();
            let _ = UiaRaiseAutomationEvent(&el, UIA_AutomationFocusChangedEventId);
        }
    }
}

fn start_fragment(hwnd: isize, code: usize) -> IRawElementProviderFragment {
    StartItem { hwnd, code }.into()
}

/// Element an Position `i` der aktuellen Elementliste.
fn start_at(hwnd: isize, nodes: &[crate::start::UiNode], i: usize) -> Result<IRawElementProviderFragment> {
    match nodes.get(i) {
        Some(n) => Ok(start_fragment(hwnd, n.code)),
        None => none(),
    }
}

#[implement(IRawElementProviderSimple, IRawElementProviderFragment, IRawElementProviderFragmentRoot)]
struct StartRoot {
    hwnd: isize,
}

impl IRawElementProviderSimple_Impl for StartRoot_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider | ProviderOptions_UseComThreading)
    }
    fn GetPatternProvider(&self, _: UIA_PATTERN_ID) -> Result<IUnknown> {
        none()
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        Ok(match id {
            UIA_NamePropertyId => VARIANT::from("Start"),
            UIA_ControlTypePropertyId => VARIANT::from(UIA_PaneControlTypeId.0),
            UIA_AutomationIdPropertyId => VARIANT::from("LightbarStart"),
            _ => VARIANT::default(),
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        unsafe { UiaHostProviderFromHwnd(HWND(self.hwnd as _)) }
    }
}

impl IRawElementProviderFragment_Impl for StartRoot_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let nodes = crate::start::ui_nodes();
        match direction {
            NavigateDirection_FirstChild => start_at(self.hwnd, &nodes, 0),
            NavigateDirection_LastChild if !nodes.is_empty() => start_at(self.hwnd, &nodes, nodes.len() - 1),
            _ => none(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        Ok(UiaRect::default())
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(StartRoot { hwnd: self.hwnd }.into())
    }
}

impl IRawElementProviderFragmentRoot_Impl for StartRoot_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let (x, y) = (x as i32, y as i32);
        let nodes = crate::start::ui_nodes();
        // Von hinten: später Gezeichnetes liegt oben.
        match nodes.iter().rev().find(|n| x >= n.rect.left && x < n.rect.right && y >= n.rect.top && y < n.rect.bottom) {
            Some(n) => Ok(start_fragment(self.hwnd, n.code)),
            None => none(),
        }
    }
    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        match crate::start::ui_focus() {
            Some(code) => Ok(start_fragment(self.hwnd, code)),
            None => none(),
        }
    }
}

#[implement(IRawElementProviderSimple, IRawElementProviderFragment, IInvokeProvider, IValueProvider)]
struct StartItem {
    hwnd: isize,
    code: usize,
}

impl StartItem_Impl {
    fn node(&self) -> Result<crate::start::UiNode> {
        match crate::start::ui_nodes().into_iter().find(|n| n.code == self.code) {
            Some(n) => Ok(n),
            None => gone(),
        }
    }
    fn is_edit(&self) -> bool {
        self.code == crate::start::Focus::Search.code()
    }
    fn copy(&self) -> StartItem {
        StartItem { hwnd: self.hwnd, code: self.code }
    }
}

impl IRawElementProviderSimple_Impl for StartItem_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider | ProviderOptions_UseComThreading)
    }
    fn GetPatternProvider(&self, pattern: UIA_PATTERN_ID) -> Result<IUnknown> {
        if pattern == UIA_ValuePatternId && self.is_edit() {
            let value: IValueProvider = self.copy().into();
            value.cast()
        } else if pattern == UIA_InvokePatternId && !self.is_edit() {
            let invoke: IInvokeProvider = self.copy().into();
            invoke.cast()
        } else {
            none()
        }
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        use crate::start::Role;
        // Fokus-Ereignisse gibt es auch für gerade nicht gezeichnete
        // (weggescrollte) Elemente – dann ohne Namen, aber mit Rolle.
        let node = self.node().ok();
        let edit = self.is_edit();
        let role = match node.as_ref().map(|n| n.role) {
            Some(Role::Edit) => UIA_EditControlTypeId,
            Some(Role::Button) => UIA_ButtonControlTypeId,
            _ if edit => UIA_EditControlTypeId,
            _ => UIA_ListItemControlTypeId,
        };
        Ok(match id {
            UIA_NamePropertyId => VARIANT::from(node.as_ref().map(|n| n.name.as_str()).unwrap_or("")),
            UIA_ControlTypePropertyId => VARIANT::from(role.0),
            UIA_AutomationIdPropertyId => VARIANT::from(format!("start-{:x}", self.code).as_str()),
            UIA_IsKeyboardFocusablePropertyId | UIA_IsEnabledPropertyId => VARIANT::from(true),
            UIA_IsInvokePatternAvailablePropertyId => VARIANT::from(!edit),
            UIA_IsValuePatternAvailablePropertyId => VARIANT::from(edit),
            UIA_ValueValuePropertyId if edit => VARIANT::from(crate::start::ui_query().as_str()),
            UIA_ValueIsReadOnlyPropertyId if edit => VARIANT::from(false),
            UIA_HasKeyboardFocusPropertyId => VARIANT::from(crate::start::ui_focus() == Some(self.code)),
            UIA_IsOffscreenPropertyId => VARIANT::from(node.is_none()),
            _ => VARIANT::default(),
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        none()
    }
}

impl IRawElementProviderFragment_Impl for StartItem_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        if direction == NavigateDirection_Parent {
            return Ok(StartRoot { hwnd: self.hwnd }.into());
        }
        let nodes = crate::start::ui_nodes();
        let Some(i) = nodes.iter().position(|n| n.code == self.code) else { return none() };
        match direction {
            NavigateDirection_NextSibling => start_at(self.hwnd, &nodes, i + 1),
            NavigateDirection_PreviousSibling if i > 0 => start_at(self.hwnd, &nodes, i - 1),
            _ => none(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        unsafe {
            let sa = SafeArrayCreateVector(VT_I4, 0, 2);
            for (i, v) in [UiaAppendRuntimeId as i32, self.code as i32 + 1].iter().enumerate() {
                SafeArrayPutElement(sa, &(i as i32), v as *const i32 as *const _)?;
            }
            Ok(sa)
        }
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        // Weggescrollt: leeres Rechteck statt Fehler.
        let Ok(n) = self.node() else { return Ok(UiaRect::default()) };
        let r = n.rect;
        Ok(UiaRect { left: r.left as f64, top: r.top as f64, width: (r.right - r.left) as f64, height: (r.bottom - r.top) as f64 })
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        crate::start::ui_set_focus(self.code);
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(StartRoot { hwnd: self.hwnd }.into())
    }
}

impl IInvokeProvider_Impl for StartItem_Impl {
    fn Invoke(&self) -> Result<()> {
        self.node()?;
        crate::start::ui_invoke(self.code);
        Ok(())
    }
}

impl IValueProvider_Impl for StartItem_Impl {
    fn SetValue(&self, val: &windows::core::PCWSTR) -> Result<()> {
        let text = unsafe { val.to_string() }.unwrap_or_default();
        crate::start::ui_set_query(&text);
        Ok(())
    }
    fn Value(&self) -> Result<windows::core::BSTR> {
        Ok(windows::core::BSTR::from(crate::start::ui_query()))
    }
    fn IsReadOnly(&self) -> Result<windows::Win32::Foundation::BOOL> {
        Ok(false.into())
    }
}
