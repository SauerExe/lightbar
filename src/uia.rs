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
