//! A UI Automation provider that exposes the switcher overlay as a real list.
//!
//! Without this, the overlay is just pixels: it draws icons and nothing else,
//! so a screen reader has nothing to read and object navigation finds an empty
//! window. Here the overlay is described as a `List` whose children are
//! `ListItem`s named after the windows they represent, with the current
//! selection exposed through the Selection patterns. That is the same shape the
//! Windows Alt-Tab switcher presents, so NVDA, Narrator and JAWS can explore it
//! and announce the selection as it moves.
//!
//! Ownership note: the root holds plain data, never child objects, and each
//! child holds a strong reference to the root. That keeps navigation cheap
//! (children are built on demand) and, more importantly, avoids a reference
//! cycle that would leak both objects.

use std::cell::RefCell;

use windows::core::{implement, IUnknown, IUnknownImpl, Interface, Result, BSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::Accessibility::{
    IRawElementProviderFragment, IRawElementProviderFragmentRoot,
    IRawElementProviderFragmentRoot_Impl, IRawElementProviderFragment_Impl,
    IRawElementProviderSimple, IRawElementProviderSimple_Impl, ISelectionItemProvider,
    ISelectionItemProvider_Impl, ISelectionProvider, ISelectionProvider_Impl, NavigateDirection,
    NavigateDirection_FirstChild, NavigateDirection_LastChild, NavigateDirection_NextSibling,
    NavigateDirection_Parent, NavigateDirection_PreviousSibling, NotificationKind_Other,
    NotificationProcessing_MostRecent, ProviderOptions, ProviderOptions_ServerSideProvider,
    UIA_AutomationFocusChangedEventId, UIA_ControlTypePropertyId, UIA_HasKeyboardFocusPropertyId,
    UIA_IsContentElementPropertyId, UIA_IsControlElementPropertyId,
    UIA_IsKeyboardFocusablePropertyId, UIA_ListControlTypeId, UIA_ListItemControlTypeId,
    UIA_NamePropertyId, UIA_SelectionItemPatternId, UIA_SelectionItem_ElementSelectedEventId,
    UIA_SelectionPatternId, UiaAppendRuntimeId, UiaGetReservedNotSupportedValue,
    UiaHostProviderFromHwnd, UiaRaiseAutomationEvent, UiaRaiseNotificationEvent, UiaRect,
    UIA_PATTERN_ID, UIA_PROPERTY_ID,
};

/// What the overlay currently shows: one entry per app, plus which is selected.
#[derive(Default)]
pub struct Selection {
    /// Accessible name of each item, in the order they are drawn.
    pub names: Vec<String>,
    /// Screen rectangle of each item, parallel to `names`.
    pub rects: Vec<RECT>,
    pub selected: usize,
}

/// Build a SAFEARRAY of i32, the shape UIA expects for runtime ids.
unsafe fn i32_safearray(values: &[i32]) -> Result<*mut SAFEARRAY> {
    unsafe {
        let array = SafeArrayCreateVector(VT_I4, 0, values.len() as u32);
        if array.is_null() {
            return Err(windows::core::Error::from_win32());
        }
        for (i, value) in values.iter().enumerate() {
            let index = i as i32;
            SafeArrayPutElement(array, &index, value as *const i32 as *const _)?;
        }
        Ok(array)
    }
}

/// The value UI Automation expects for a property a provider does not
/// implement. Returning an empty variant instead makes clients treat it as a
/// real, zero-valued answer - which is how a plain list ends up being read out
/// with invented grid coordinates like "row 2, column 3".
fn not_supported() -> VARIANT {
    unsafe {
        match UiaGetReservedNotSupportedValue() {
            Ok(unknown) => VARIANT::from(unknown),
            Err(err) => {
                debug!("uia: reserved not-supported value unavailable, {err}");
                VARIANT::default()
            }
        }
    }
}

fn to_uia_rect(rect: &RECT) -> UiaRect {
    UiaRect {
        left: rect.left as f64,
        top: rect.top as f64,
        width: (rect.right - rect.left) as f64,
        height: (rect.bottom - rect.top) as f64,
    }
}

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IRawElementProviderFragmentRoot,
    ISelectionProvider
)]
pub struct ListProvider {
    hwnd: HWND,
    selection: RefCell<Selection>,
}

impl ListProvider {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            selection: RefCell::new(Selection::default()),
        }
    }

    pub fn set_selection(&self, selection: Selection) {
        *self.selection.borrow_mut() = selection;
    }

    pub fn set_selected_index(&self, index: usize) {
        self.selection.borrow_mut().selected = index;
    }

    pub fn clear(&self) {
        *self.selection.borrow_mut() = Selection::default();
    }

    pub fn count(&self) -> usize {
        self.selection.borrow().names.len()
    }

    fn item(root: &ComObjectRoot, index: usize) -> Option<IRawElementProviderFragment> {
        if index >= root.count() {
            return None;
        }
        let item: ItemProvider = ItemProvider {
            root: root.clone(),
            index,
        };
        Some(windows::core::ComObject::new(item).to_interface())
    }

    /// The provider for the item that is currently selected, used to target
    /// focus and selection events.
    pub fn selected_item(root: &ComObjectRoot) -> Option<IRawElementProviderSimple> {
        let index = root.selection.borrow().selected;
        if index >= root.count() {
            return None;
        }
        let item: ItemProvider = ItemProvider {
            root: root.clone(),
            index,
        };
        Some(windows::core::ComObject::new(item).to_interface())
    }
}

/// Shorthand for the reference-counted root object shared with its children.
pub type ComObjectRoot = windows::core::ComObject<ListProvider>;

impl IRawElementProviderSimple_Impl for ListProvider_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, patternid: UIA_PATTERN_ID) -> Result<IUnknown> {
        if patternid == UIA_SelectionPatternId {
            let unknown: IUnknown = self.to_interface::<ISelectionProvider>().cast()?;
            return Ok(unknown);
        }
        Err(windows::core::Error::empty())
    }

    fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let value = match propertyid {
            id if id == UIA_ControlTypePropertyId => VARIANT::from(UIA_ListControlTypeId.0),
            // Deliberately nameless: this container is a means to an end, and
            // naming it only makes a screen reader say "Window Switcher" before
            // every switch. The items carry the names worth hearing.
            id if id == UIA_NamePropertyId => VARIANT::from(BSTR::new()),
            id if id == UIA_IsControlElementPropertyId => VARIANT::from(true),
            id if id == UIA_IsContentElementPropertyId => VARIANT::from(true),
            id if id == UIA_IsKeyboardFocusablePropertyId => VARIANT::from(true),
            _ => not_supported(),
        };
        Ok(value)
    }

    /// Chaining to the window's host provider lets UIA supply the window-level
    /// properties (bounds, process id, native handle) for free.
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        unsafe { UiaHostProviderFromHwnd(self.hwnd) }
    }
}

impl IRawElementProviderFragment_Impl for ListProvider_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        // The root's parent is the window itself, which UIA derives from the
        // host provider, so we report nothing and let it fill the gap.
        let root = self.to_object();
        let result = if direction == NavigateDirection_FirstChild {
            ListProvider::item(&root, 0)
        } else if direction == NavigateDirection_LastChild {
            ListProvider::item(&root, root.count().saturating_sub(1))
        } else {
            None
        };
        result.ok_or_else(windows::core::Error::empty)
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // NULL, not an empty array: it tells UI Automation to identify this
        // fragment root by its window. An empty array is a real (empty) answer
        // instead, which leaves the children's ids - built by appending to the
        // root's - unresolvable, so events raised on them cannot be routed to a
        // client and a screen reader never hears about the selection moving.
        Ok(std::ptr::null_mut())
    }

    fn BoundingRectangle(&self) -> Result<UiaRect> {
        // Deferring to the host provider keeps the root aligned with the window.
        Ok(UiaRect::default())
    }

    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }

    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }

    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(self.to_interface())
    }
}

impl IRawElementProviderFragmentRoot_Impl for ListProvider_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let root = self.to_object();
        let index = {
            let selection = self.selection.borrow();
            selection.rects.iter().position(|rect| {
                (x as i32) >= rect.left
                    && (x as i32) < rect.right
                    && (y as i32) >= rect.top
                    && (y as i32) < rect.bottom
            })
        };
        index
            .and_then(|index| ListProvider::item(&root, index))
            .ok_or_else(windows::core::Error::empty)
    }

    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        let root = self.to_object();
        let index = self.selection.borrow().selected;
        ListProvider::item(&root, index).ok_or_else(windows::core::Error::empty)
    }
}

impl ISelectionProvider_Impl for ListProvider_Impl {
    fn GetSelection(&self) -> Result<*mut SAFEARRAY> {
        // Reporting the selection as an element array is optional for screen
        // readers; the per-item SelectionItem pattern carries the information
        // they actually read, so an empty array keeps this simple and safe.
        unsafe { i32_safearray(&[]) }
    }

    fn CanSelectMultiple(&self) -> Result<windows::core::BOOL> {
        Ok(false.into())
    }

    fn IsSelectionRequired(&self) -> Result<windows::core::BOOL> {
        Ok(true.into())
    }
}

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    ISelectionItemProvider
)]
pub struct ItemProvider {
    root: ComObjectRoot,
    index: usize,
}

impl ItemProvider {
    fn name(&self) -> String {
        self.root
            .selection
            .borrow()
            .names
            .get(self.index)
            .cloned()
            .unwrap_or_default()
    }

    fn is_selected(&self) -> bool {
        self.root.selection.borrow().selected == self.index
    }
}

impl IRawElementProviderSimple_Impl for ItemProvider_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, patternid: UIA_PATTERN_ID) -> Result<IUnknown> {
        if patternid == UIA_SelectionItemPatternId {
            let unknown: IUnknown = self.to_interface::<ISelectionItemProvider>().cast()?;
            return Ok(unknown);
        }
        Err(windows::core::Error::empty())
    }

    fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let value = match propertyid {
            id if id == UIA_ControlTypePropertyId => VARIANT::from(UIA_ListItemControlTypeId.0),
            id if id == UIA_NamePropertyId => VARIANT::from(BSTR::from(self.name())),
            id if id == UIA_IsControlElementPropertyId => VARIANT::from(true),
            id if id == UIA_IsContentElementPropertyId => VARIANT::from(true),
            id if id == UIA_IsKeyboardFocusablePropertyId => VARIANT::from(true),
            id if id == UIA_HasKeyboardFocusPropertyId => VARIANT::from(self.is_selected()),
            _ => not_supported(),
        };
        Ok(value)
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        // Items are not windows of their own.
        Err(windows::core::Error::empty())
    }
}

impl IRawElementProviderFragment_Impl for ItemProvider_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let root = &self.root;
        let count = root.count();
        let result = if direction == NavigateDirection_Parent {
            Some(root.to_interface())
        } else if direction == NavigateDirection_NextSibling && self.index + 1 < count {
            ListProvider::item(root, self.index + 1)
        } else if direction == NavigateDirection_PreviousSibling && self.index > 0 {
            ListProvider::item(root, self.index - 1)
        } else {
            None
        };
        result.ok_or_else(windows::core::Error::empty)
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // Prefixing with UiaAppendRuntimeId makes the id unique per window, and
        // the index keeps it stable while the switcher is open, so UIA can tell
        // the items apart across queries.
        unsafe { i32_safearray(&[UiaAppendRuntimeId as i32, self.index as i32]) }
    }

    fn BoundingRectangle(&self) -> Result<UiaRect> {
        // The painter already lays items out in screen coordinates, which is
        // what UIA wants, so no conversion is needed.
        let rect = self
            .root
            .selection
            .borrow()
            .rects
            .get(self.index)
            .copied()
            .unwrap_or_default();
        Ok(to_uia_rect(&rect))
    }

    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }

    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }

    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(self.root.to_interface())
    }
}

impl ISelectionItemProvider_Impl for ItemProvider_Impl {
    fn Select(&self) -> Result<()> {
        self.root.set_selected_index(self.index);
        Ok(())
    }

    fn AddToSelection(&self) -> Result<()> {
        self.Select()
    }

    fn RemoveFromSelection(&self) -> Result<()> {
        // Single-selection list: the selection can move but never be emptied.
        Err(windows::core::Error::empty())
    }

    fn IsSelected(&self) -> Result<windows::core::BOOL> {
        Ok(self.is_selected().into())
    }

    fn SelectionContainer(&self) -> Result<IRawElementProviderSimple> {
        Ok(self.root.to_interface())
    }
}

/// Tell assistive technology that the selection moved to the current item.
///
/// Both events are raised because screen readers differ in which they act on:
/// NVDA follows the focus-changed event, while others track element-selected.
/// Ask the screen reader to speak `text`.
///
/// Raised on our own selected item rather than on the window's generic host
/// provider, so the notification arrives attached to the element it is about.
pub fn notify(root: &ComObjectRoot, text: &str) {
    let target: IRawElementProviderSimple = match ListProvider::selected_item(root) {
        Some(item) => item,
        None => root.to_interface(),
    };
    unsafe {
        if let Err(err) = UiaRaiseNotificationEvent(
            &target,
            NotificationKind_Other,
            NotificationProcessing_MostRecent,
            &BSTR::from(text),
            &BSTR::from("WindowSwitcherSelection"),
        ) {
            debug!("uia: notification failed, {err}");
        }
    }
}

pub fn raise_selection_events(root: &ComObjectRoot) {
    let Some(item) = ListProvider::selected_item(root) else {
        return;
    };
    unsafe {
        if let Err(err) = UiaRaiseAutomationEvent(&item, UIA_AutomationFocusChangedEventId) {
            debug!("uia: focus event failed, {err}");
        }
        if let Err(err) = UiaRaiseAutomationEvent(&item, UIA_SelectionItem_ElementSelectedEventId) {
            debug!("uia: selection event failed, {err}");
        }
    }
}
