//! Exact Windows UIA setup for Chromium existing-profile attachment.

use std::time::{Duration, Instant};
use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
};

use cua_driver_core::browser::{
    BrowserRefusal, BrowserRefusalCode, BrowserSetupDescriptor,
    EXISTING_PROFILE_SETUP_READY_TIMEOUT,
};
use windows::core::{AgileReference, Interface, BSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetElemsize,
    SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayGetVartype,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VT_I4;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationTogglePattern, IUIAutomationValuePattern, ToggleState_Off, ToggleState_On,
    UIA_ButtonControlTypeId, UIA_InvokePatternId, UIA_TabItemControlTypeId, UIA_TogglePatternId,
    UIA_ValuePatternId, UIA_WindowControlTypeId, UIA_CONTROLTYPE_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};

use crate::uia::UiaNode;

// Native Chromium chrome is localized, so its accessible names are diagnostic
// text rather than a stable automation contract. Bootstrap against the exact
// approved HWND, native-vs-renderer boundary, control type, supported action,
// uniqueness, and exact post-action state instead of maintaining language
// allowlists or accepting fuzzy labels. The internal setup page follows the
// same rule: its native URL and web-control topology are contracts; its
// localized document, heading, and checkbox names are not.

fn refusal(code: BrowserRefusalCode, message: impl Into<String>) -> BrowserRefusal {
    BrowserRefusal::new(code, message)
}

fn release_nodes(nodes: &[UiaNode]) {
    for node in nodes.iter().filter(|node| node.element_ptr != 0) {
        unsafe { drop(IUIAutomationElement::from_raw(node.element_ptr as *mut _)) };
    }
}

/// Browser chrome only: tab strip, toolbar, omnibox, and the active tab's
/// `Document` node without its renderer-owned descendants. Tab topology and
/// the committed setup URL are native facts, so reading them must not depend
/// on the page renderer answering UIA (#4121).
const NATIVE_READ_RETRIES: usize = 3;

fn native_nodes(hwnd: u64) -> Result<Vec<UiaNode>, BrowserRefusal> {
    // A UIA read can fail transiently while Chromium rebuilds its tree (for
    // example "Malformed Cacheresponse" as a native prompt opens). Retry a
    // few times before refusing.
    let mut attempt = 0;
    let walked = loop {
        match crate::uia::walk_native_tree_complete(hwnd) {
            Err(_) if attempt < NATIVE_READ_RETRIES => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            result => break result,
        }
    };
    walked.map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!(
                "the approved browser window's native accessibility tree was unavailable: {error}"
            ),
        )
    })
}

const MAX_RUNTIME_ID_PARTS: usize = 1024;

struct SetupComScope;

impl SetupComScope {
    fn new() -> Result<Self, BrowserRefusal> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| {
                refusal(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    format!("native setup cleanup COM initialization failed: {error}"),
                )
            })?;
        Ok(Self)
    }
}

impl Drop for SetupComScope {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// UIA IDs are opaque and may be reused after an element dies. They identify
/// rows in complete snapshots; retained agile COM identities below separately
/// prove that the original native elements are still alive before input.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RuntimeId(Vec<i32>);

impl RuntimeId {
    fn new(parts: Vec<i32>) -> Result<Self, BrowserRefusal> {
        if parts.is_empty() || parts.len() > MAX_RUNTIME_ID_PARTS {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingAmbiguous,
                "the native element did not expose one complete bounded runtime ID",
            ));
        }
        Ok(Self(parts))
    }
}

struct RuntimeIdArray(*mut SAFEARRAY);

impl Drop for RuntimeIdArray {
    fn drop(&mut self) {
        unsafe {
            let _ = SafeArrayDestroy(self.0);
        }
    }
}

fn element_runtime_id(element: &IUIAutomationElement) -> Result<RuntimeId, BrowserRefusal> {
    let read = || unsafe {
        let array = RuntimeIdArray(element.GetRuntimeId()?);
        if array.0.is_null()
            || SafeArrayGetDim(array.0) != 1
            || SafeArrayGetVartype(array.0)? != VT_I4
            || SafeArrayGetElemsize(array.0) as usize != std::mem::size_of::<i32>()
        {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80070057u32 as i32,
            )));
        }
        let lower = SafeArrayGetLBound(array.0, 1)?;
        let upper = SafeArrayGetUBound(array.0, 1)?;
        let count = i64::from(upper) - i64::from(lower) + 1;
        if !(1..=MAX_RUNTIME_ID_PARTS as i64).contains(&count) {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80070057u32 as i32,
            )));
        }
        let mut parts = Vec::with_capacity(count as usize);
        for index in lower..=upper {
            let mut part = 0i32;
            SafeArrayGetElement(array.0, &index, (&mut part as *mut i32).cast())?;
            parts.push(part);
        }
        Ok(parts)
    };
    RuntimeId::new(read().map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserBindingStale,
            format!("the complete native runtime ID was unavailable: {error}"),
        )
    })?)
}

fn runtime_id(element_ptr: usize) -> Result<RuntimeId, BrowserRefusal> {
    if element_ptr == 0 {
        return Err(refusal(
            BrowserRefusalCode::BrowserBindingStale,
            "the native element was missing",
        ));
    }
    let element = std::mem::ManuallyDrop::new(unsafe {
        IUIAutomationElement::from_raw(element_ptr as *mut _)
    });
    element_runtime_id(&element)
}

#[derive(Clone, Debug)]
struct NativeTab {
    id: RuntimeId,
    node_index: usize,
}

fn native_tab_identities(
    nodes: &[UiaNode],
    mut read_id: impl FnMut(usize) -> Result<RuntimeId, BrowserRefusal>,
) -> Result<Vec<NativeTab>, BrowserRefusal> {
    let mut seen = HashSet::new();
    let mut tabs = Vec::new();
    // The Document row is only the renderer boundary, never ownership or an
    // action target. Do not ask that renderer for identity during cleanup.
    // Every native chrome row, including inert containers and foreign close
    // buttons, participates in uniqueness so a reused close ID fails closed.
    for (node_index, node) in nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| !node.in_web_content && node.control_type != "Document")
    {
        if node.element_ptr == 0 {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "a native chrome identity was unavailable",
            ));
        }
        let id = read_id(node.element_ptr)?;
        if !seen.insert(id.clone()) {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingAmbiguous,
                "the complete native chrome topology contained duplicate runtime IDs",
            ));
        }
        if node.control_type == "TabItem" {
            tabs.push(NativeTab { id, node_index });
        }
    }
    Ok(tabs)
}

fn select_new_native_tab(
    before: &[NativeTab],
    after: &[NativeTab],
) -> Result<Option<usize>, BrowserRefusal> {
    if before.is_empty()
        || before
            .iter()
            .any(|old| !after.iter().any(|new| old.id == new.id))
    {
        return Err(refusal(
            BrowserRefusalCode::BrowserBindingAmbiguous,
            "the original native tab identities changed during setup",
        ));
    }
    let new = after
        .iter()
        .filter(|new| !before.iter().any(|old| old.id == new.id))
        .collect::<Vec<_>>();
    match new.as_slice() {
        [] => Ok(None),
        [tab] => Ok(Some(tab.node_index)),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserBindingAmbiguous,
            "setup did not create exactly one independently identified native tab",
        )),
    }
}

fn select_new_native_tab_with_baseline(
    before: &[NativeTab],
    after: &[NativeTab],
    mut verify_original: impl FnMut(&NativeTab, &NativeTab) -> Result<(), BrowserRefusal>,
) -> Result<Option<usize>, BrowserRefusal> {
    let selected = select_new_native_tab(before, after)?;
    for original in before {
        let current = after
            .iter()
            .find(|current| current.id == original.id)
            .ok_or_else(|| {
                refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    "an original native tab disappeared during setup",
                )
            })?;
        verify_original(original, current)?;
    }
    Ok(selected)
}

fn unique_tab_close_button(nodes: &[UiaNode], tab_index: usize) -> Result<usize, BrowserRefusal> {
    let tab = &nodes[tab_index];
    let buttons = nodes
        .iter()
        .enumerate()
        .skip(tab_index + 1)
        .take_while(|(_, node)| node.depth > tab.depth)
        .filter(|(_, node)| {
            !node.in_web_content
                && node.control_type == "Button"
                && node.enabled == Some(true)
                && node.element_ptr != 0
                && node.actions.iter().any(|action| action == "invoke")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match buttons.as_slice() {
        [index] => Ok(*index),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserBindingAmbiguous,
            "the owned native tab did not expose exactly one native close action",
        )),
    }
}

#[derive(Clone, Debug)]
struct OwnedTabProof {
    tab: RuntimeId,
    close: RuntimeId,
}

impl OwnedTabProof {
    #[cfg(test)]
    fn close_button(
        &self,
        nodes: &[UiaNode],
        read_id: impl FnMut(usize) -> Result<RuntimeId, BrowserRefusal>,
    ) -> Result<usize, BrowserRefusal> {
        self.locate(nodes, read_id).map(|(_, _, index)| index)
    }

    fn locate(
        &self,
        nodes: &[UiaNode],
        mut read_id: impl FnMut(usize) -> Result<RuntimeId, BrowserRefusal>,
    ) -> Result<(Vec<NativeTab>, usize, usize), BrowserRefusal> {
        let tabs = native_tab_identities(nodes, &mut read_id)?;
        let tab = tabs.iter().find(|tab| tab.id == self.tab).ok_or_else(|| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the originally owned native tab was no longer present",
            )
        })?;
        let index = unique_tab_close_button(nodes, tab.node_index)?;
        if tabs.iter().any(|tab| tab.id == self.close) {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingAmbiguous,
                "the close action reused a native tab identity",
            ));
        }
        if read_id(nodes[index].element_ptr)? != self.close {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the owned native tab's original close action changed",
            ));
        }
        let tab_index = tab.node_index;
        Ok((tabs, tab_index, index))
    }
}

fn owned_tab_disappeared(proof: &OwnedTabProof, before: &[NativeTab], after: &[NativeTab]) -> bool {
    !after.iter().any(|tab| tab.id == proof.tab)
        && before
            .iter()
            .filter(|tab| tab.id != proof.tab)
            .all(|old| after.iter().any(|new| old.id == new.id))
}

fn invoke_owned_close_once<T>(
    proven: Result<T, BrowserRefusal>,
    invoke_close: impl FnOnce(&T) -> Result<(), BrowserRefusal>,
    readback: impl FnOnce(&T) -> Result<bool, BrowserRefusal>,
) -> Result<bool, BrowserRefusal> {
    let proven = proven?;
    invoke_close(&proven)?;
    readback(&proven)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SetupWindowIdentity {
    hwnd: u64,
    pid: u32,
    birth: u64,
}

impl SetupWindowIdentity {
    fn capture(hwnd: u64) -> Result<Self, BrowserRefusal> {
        let target = HWND(hwnd as *mut _);
        let mut pid = 0;
        if !unsafe { IsWindow(target) }.as_bool()
            || unsafe { GetWindowThreadProcessId(target, Some(&mut pid)) } == 0
            || pid == 0
        {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the exact setup window was unavailable",
            ));
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|error| {
                refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    format!("the exact setup process was unavailable: {error}"),
                )
            })?;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let times =
            unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
        unsafe {
            let _ = CloseHandle(process);
        }
        times.map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the exact setup process birth was unavailable: {error}"),
            )
        })?;
        Ok(Self {
            hwnd,
            pid,
            birth: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
        })
    }

    fn revalidate(self) -> Result<(), BrowserRefusal> {
        if Self::capture(self.hwnd)? != self {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the exact setup window/process birth changed",
            ));
        }
        Ok(())
    }
}

struct OwnedNativeElement {
    id: RuntimeId,
    element: AgileReference<IUIAutomationElement>,
}

impl OwnedNativeElement {
    fn capture(node: &UiaNode) -> Result<Self, BrowserRefusal> {
        let id = runtime_id(node.element_ptr)?;
        let element = std::mem::ManuallyDrop::new(unsafe {
            IUIAutomationElement::from_raw(node.element_ptr as *mut _)
        });
        let element = AgileReference::new(&*element).map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the native setup identity could not be retained safely: {error}"),
            )
        })?;
        Ok(Self { id, element })
    }

    fn matches(
        &self,
        node: &UiaNode,
        automation: &IUIAutomation,
        window: SetupWindowIdentity,
        kind: UIA_CONTROLTYPE_ID,
    ) -> Result<(), BrowserRefusal> {
        let original = self.element.resolve().map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the retained native setup element could not be resolved: {error}"),
            )
        })?;
        if element_runtime_id(&original)? != self.id {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the retained native setup element's runtime ID changed",
            ));
        }
        let compare = || unsafe {
            let current = std::mem::ManuallyDrop::new(IUIAutomationElement::from_raw(
                node.element_ptr as *mut _,
            ));
            Ok(original.CurrentControlType()? == kind
                && current.CurrentControlType()? == kind
                && original.CurrentProcessId()? == window.pid as i32
                && current.CurrentProcessId()? == window.pid as i32
                && automation.CompareElements(&original, &*current)?.as_bool())
        };
        if runtime_id(node.element_ptr)? != self.id
            || !compare().map_err(|error: windows::core::Error| {
                refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    format!("the original native setup element was no longer alive: {error}"),
                )
            })?
        {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the original native setup element changed",
            ));
        }
        Ok(())
    }
}

fn native_window_node(nodes: &[UiaNode]) -> Result<&UiaNode, BrowserRefusal> {
    nodes
        .iter()
        .find(|node| {
            node.depth == 0
                && !node.in_web_content
                && node.control_type == "Window"
                && node.element_ptr != 0
        })
        .ok_or_else(|| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the complete native tree had no exact window root",
            )
        })
}

fn prove_native_baseline(
    original_window: &OwnedNativeElement,
    originals: &[OwnedNativeElement],
    before: &[NativeTab],
    after: &[NativeTab],
    nodes: &[UiaNode],
    window: SetupWindowIdentity,
) -> Result<Option<usize>, BrowserRefusal> {
    window.revalidate()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.map_err(
            |error| {
                refusal(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    format!("native baseline identity comparison was unavailable: {error}"),
                )
            },
        )?;
    original_window.matches(
        native_window_node(nodes)?,
        &automation,
        window,
        UIA_WindowControlTypeId,
    )?;
    select_new_native_tab_with_baseline(before, after, |original, current| {
        let retained = originals
            .iter()
            .find(|element| element.id == original.id)
            .ok_or_else(|| {
                refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    "an original native tab had no retained ownership identity",
                )
            })?;
        retained.matches(
            &nodes[current.node_index],
            &automation,
            window,
            UIA_TabItemControlTypeId,
        )
    })
}

struct OwnedSetupTab {
    proof: OwnedTabProof,
    window: OwnedNativeElement,
    tab: OwnedNativeElement,
    close: OwnedNativeElement,
}

struct ProvenOwnedTab {
    tabs: Vec<NativeTab>,
    tab_index: usize,
    close_index: usize,
    automation: IUIAutomation,
}

impl OwnedSetupTab {
    fn capture(
        nodes: &[UiaNode],
        tab_index: usize,
        window: OwnedNativeElement,
    ) -> Result<Self, BrowserRefusal> {
        let close_index = unique_tab_close_button(nodes, tab_index)?;
        let tab = OwnedNativeElement::capture(&nodes[tab_index])?;
        let close = OwnedNativeElement::capture(&nodes[close_index])?;
        if tab.id == close.id {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingAmbiguous,
                "the native tab and close action shared an invalid identity",
            ));
        }
        Ok(Self {
            proof: OwnedTabProof {
                tab: tab.id.clone(),
                close: close.id.clone(),
            },
            window,
            tab,
            close,
        })
    }

    fn prove(
        &self,
        nodes: &[UiaNode],
        window: SetupWindowIdentity,
    ) -> Result<ProvenOwnedTab, BrowserRefusal> {
        window.revalidate()?;
        let (tabs, tab_index, close_index) = self.proof.locate(nodes, runtime_id)?;
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.map_err(
                |error| {
                    refusal(
                        BrowserRefusalCode::BrowserRouteUnavailable,
                        format!("native setup identity comparison was unavailable: {error}"),
                    )
                },
            )?;
        self.window.matches(
            native_window_node(nodes)?,
            &automation,
            window,
            UIA_WindowControlTypeId,
        )?;
        self.tab.matches(
            &nodes[tab_index],
            &automation,
            window,
            UIA_TabItemControlTypeId,
        )?;
        self.close.matches(
            &nodes[close_index],
            &automation,
            window,
            UIA_ButtonControlTypeId,
        )?;
        window.revalidate()?;
        Ok(ProvenOwnedTab {
            tabs,
            tab_index,
            close_index,
            automation,
        })
    }

    fn active(&self, window: SetupWindowIdentity) -> Result<(), BrowserRefusal> {
        let nodes = native_nodes(window.hwnd)?;
        let result = self.prove(&nodes, window).and_then(|proven| {
            if nodes[proven.tab_index].selected == Some(true) {
                Ok(())
            } else {
                Err(refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    "the owned setup tab was no longer the selected native tab",
                ))
            }
        });
        release_nodes(&nodes);
        result
    }

    fn close(&self, window: SetupWindowIdentity) -> Result<bool, BrowserRefusal> {
        let _com = SetupComScope::new()?;
        window.revalidate()?;
        let nodes = native_nodes(window.hwnd)?;
        let proven = self.prove(&nodes, window);
        let result = invoke_owned_close_once(
            proven,
            |proven| {
                window.revalidate()?;
                unsafe {
                    invoke(
                        nodes[proven.close_index].element_ptr,
                        "owned native tab close button",
                    )
                }
            },
            |proven| {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    window.revalidate()?;
                    let nodes = native_nodes(window.hwnd)?;
                    let after = (|| {
                        self.window.matches(
                            native_window_node(&nodes)?,
                            &proven.automation,
                            window,
                            UIA_WindowControlTypeId,
                        )?;
                        native_tab_identities(&nodes, runtime_id)
                    })();
                    release_nodes(&nodes);
                    let disappeared = owned_tab_disappeared(&self.proof, &proven.tabs, &after?);
                    window.revalidate()?;
                    if disappeared {
                        return Ok(true);
                    }
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            },
        );
        release_nodes(&nodes);
        result
    }
}

fn unique_web_actionable(
    nodes: &[UiaNode],
    control_type: &str,
    action: &str,
) -> Result<Option<usize>, BrowserRefusal> {
    let matches = nodes
        .iter()
        .filter(|node| {
            node.in_web_content
                && node.control_type == control_type
                && node.actions.iter().any(|value| value == action)
                && node.enabled != Some(false)
                && node.element_ptr != 0
        })
        .map(|node| node.element_ptr)
        .collect::<HashSet<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!(
                "multiple web {control_type} controls expose the exact {action} action on the setup page"
            ),
        )),
    }
}

fn unique_native_actionable(
    nodes: &[UiaNode],
    control_type: &str,
    action: &str,
) -> Result<Option<usize>, BrowserRefusal> {
    unique_native_actionable_with_focus(nodes, control_type, action, element_has_keyboard_focus)
}

fn element_has_keyboard_focus(element_ptr: usize) -> bool {
    if element_ptr == 0 {
        return false;
    }
    let element = unsafe { IUIAutomationElement::from_raw(element_ptr as *mut _) };
    let focused = unsafe { element.CurrentHasKeyboardFocus() }
        .ok()
        .is_some_and(|value| value.as_bool());
    std::mem::forget(element);
    focused
}

fn unique_native_actionable_with_focus(
    nodes: &[UiaNode],
    control_type: &str,
    action: &str,
    mut has_keyboard_focus: impl FnMut(usize) -> bool,
) -> Result<Option<usize>, BrowserRefusal> {
    let matches = nodes
        .iter()
        .filter(|node| {
            !node.in_web_content
                && node.control_type == control_type
                && node.actions.iter().any(|value| value == action)
                && node.enabled != Some(false)
                && node.element_ptr != 0
        })
        .map(|node| node.element_ptr)
        .collect::<HashSet<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => {
            let focused = matches
                .into_iter()
                .filter(|element| has_keyboard_focus(*element))
                .collect::<Vec<_>>();
            match focused.as_slice() {
                [element] => Ok(Some(*element)),
                _ => Err(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    format!(
                        "multiple native {control_type} controls expose the exact {action} action, \
                         and keyboard focus did not identify exactly one"
                    ),
                )),
            }
        }
    }
}

#[cfg(test)]
fn native_tab_count(nodes: &[UiaNode]) -> usize {
    nodes
        .iter()
        .filter(|node| !node.in_web_content && node.control_type == "TabItem")
        .filter_map(|node| node.rect)
        .filter(|(left, top, right, bottom)| left < right && top < bottom)
        .collect::<std::collections::HashSet<_>>()
        .len()
}

fn exact_native_new_tab_button(nodes: &[UiaNode]) -> Result<Option<usize>, BrowserRefusal> {
    // Identity reads retain every native row. Preserve this existing action
    // selector's former display/actionable projection: an inert anonymous
    // structural sibling must not replace the original new-tab successor.
    let nodes = nodes
        .iter()
        .filter(|node| {
            (!node.actions.is_empty() && node.enabled != Some(false))
                || node
                    .name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
                || node
                    .value
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
        })
        .collect::<Vec<_>>();
    let Some(last_tab_index) = nodes.iter().rposition(|node| {
        !node.in_web_content
            && node.control_type == "TabItem"
            && node
                .rect
                .is_some_and(|(left, top, right, bottom)| left < right && top < bottom)
    }) else {
        return Ok(None);
    };
    let last_tab = &nodes[last_tab_index];
    let successor_index =
        (last_tab_index + 1..nodes.len()).find(|index| nodes[*index].depth <= last_tab.depth);
    let Some(successor) = successor_index.map(|index| &nodes[index]) else {
        return Ok(None);
    };
    let vertically_overlaps_tab_row = match (last_tab.rect, successor.rect) {
        (Some((_, tab_top, _, tab_bottom)), Some((_, button_top, _, button_bottom))) => {
            tab_top < button_bottom && button_top < tab_bottom
        }
        _ => false,
    };
    if successor.in_web_content
        || successor.control_type != "Button"
        || successor.enabled == Some(false)
        || successor.element_ptr == 0
        || !successor.actions.iter().any(|action| action == "invoke")
        || !vertically_overlaps_tab_row
    {
        return Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the native tab strip did not expose one exact structural new-tab action",
        ));
    }
    Ok(Some(successor.element_ptr))
}

fn setup_page_proven(nodes: &[UiaNode], descriptor: &BrowserSetupDescriptor) -> bool {
    let exact_url_count = nodes
        .iter()
        .filter(|node| {
            !node.in_web_content
                && node.control_type == "Edit"
                && node.actions.iter().any(|value| value == "set_value")
                && node
                    .value
                    .as_deref()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case(descriptor.setup_url))
        })
        .count();
    let document_count = nodes
        .iter()
        .filter(|node| node.control_type == "Document" && !node.in_web_content)
        .count();
    exact_url_count == 1 && document_count == 1
}

/// Whether the setup flow must read renderer-owned nodes before it acts: only
/// when the committed active tab is already the exact setup page, whose
/// checkbox lives in web content. Every other page is left unread (#4121).
fn needs_setup_page_renderer(native: &[UiaNode], descriptor: &BrowserSetupDescriptor) -> bool {
    setup_page_proven(native, descriptor)
}

fn exact_setup_checkbox(
    nodes: &[UiaNode],
    descriptor: &BrowserSetupDescriptor,
) -> Result<Option<usize>, BrowserRefusal> {
    if !setup_page_proven(nodes, descriptor) {
        return Ok(None);
    }
    unique_web_actionable(nodes, "CheckBox", "toggle")
}

unsafe fn set_value(element_ptr: usize, value: &str) -> Result<(), BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = element
        .GetCurrentPattern(UIA_ValuePatternId)
        .and_then(|pattern| pattern.cast::<IUIAutomationValuePattern>())
        .and_then(|pattern| pattern.SetValue(&BSTR::from(value)));
    std::mem::forget(element);
    result.map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("the exact UIA Value action failed: {error}"),
        )
    })
}

fn force_setup_foreground(target: windows::Win32::Foundation::HWND) -> (bool, bool) {
    unsafe { crate::input::force_foreground_assisted(target) }
}

unsafe fn invoke(element_ptr: usize, description: &str) -> Result<(), BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = element
        .GetCurrentPattern(UIA_InvokePatternId)
        .and_then(|pattern| pattern.cast::<IUIAutomationInvokePattern>())
        .and_then(|pattern| pattern.Invoke());
    std::mem::forget(element);
    result.map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("the exact {description} UIA Invoke action failed: {error}"),
        )
    })
}

fn confirm_setup_navigation(
    hwnd: u64,
    element_ptr: usize,
    foregrounded_window: &mut bool,
    injected_global_input: &mut bool,
    focused_setup_address_field: &mut bool,
) -> Result<(), BrowserRefusal> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let target = HWND(hwnd as *mut _);
    let prior = unsafe { GetForegroundWindow() };
    let navigation = (|| {
        let (fronted, injected) = force_setup_foreground(target);
        *injected_global_input |= injected;
        if !fronted {
            return Err(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "Windows refused the bounded foreground assist for the exact browser window",
            ));
        }
        *foregrounded_window = true;

        let element = unsafe { IUIAutomationElement::from_raw(element_ptr as *mut _) };
        let focused = unsafe {
            element
                .SetFocus()
                .and_then(|_| element.CurrentHasKeyboardFocus())
        };
        std::mem::forget(element);
        match focused {
            Ok(value) if value.as_bool() => *focused_setup_address_field = true,
            Ok(_) => {
                return Err(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    "the exact address-and-search field did not acquire keyboard focus",
                ));
            }
            Err(error) => {
                return Err(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    format!("could not focus the exact address-and-search field: {error}"),
                ));
            }
        }

        *injected_global_input = true;
        crate::input::keyboard::send_key_synthesized(hwnd, "enter", &[]).map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!("could not confirm the bounded setup navigation: {error}"),
            )
        })
    })();

    let restored = if prior.0.is_null() || prior == target {
        true
    } else {
        let (restored, injected) = force_setup_foreground(prior);
        *injected_global_input |= injected;
        restored
    };
    if !restored {
        return Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the setup navigation completed, but Windows refused to restore the prior foreground window",
        ));
    }
    navigation
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckboxState {
    Off,
    On,
}

fn attempt_setup_toggle(
    attempted: &mut bool,
    toggle: impl FnOnce() -> Result<(), BrowserRefusal>,
) -> Result<bool, BrowserRefusal> {
    // A failed UIA call may already have changed browser state.
    *attempted = true;
    toggle().map(|_| false)
}

fn needs_debugging_restore(
    confirmed_enabled: bool,
    attempted: bool,
    initial: Option<CheckboxState>,
) -> bool {
    confirmed_enabled || (attempted && initial == Some(CheckboxState::Off))
}

fn revalidate_rollback_context<T>(
    window: impl FnOnce() -> Result<(), BrowserRefusal>,
    owned_tab: Option<&T>,
    tab: impl FnOnce(&T) -> Result<(), BrowserRefusal>,
) -> Result<(), BrowserRefusal> {
    window()?;
    if let Some(owned) = owned_tab {
        tab(owned)?;
    }
    Ok(())
}

unsafe fn checkbox_state(element_ptr: usize) -> Result<CheckboxState, BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = element
        .GetCurrentPattern(UIA_TogglePatternId)
        .and_then(|pattern| pattern.cast::<IUIAutomationTogglePattern>())
        .and_then(|pattern| pattern.CurrentToggleState());
    std::mem::forget(element);
    match result {
        Ok(state) if state == ToggleState_Off => Ok(CheckboxState::Off),
        Ok(state) if state == ToggleState_On => Ok(CheckboxState::On),
        Ok(_) => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the exact remote-debugging checkbox had an indeterminate state",
        )),
        Err(error) => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("could not read the exact remote-debugging checkbox: {error}"),
        )),
    }
}

unsafe fn toggle(element_ptr: usize) -> Result<(), BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = element
        .GetCurrentPattern(UIA_TogglePatternId)
        .and_then(|pattern| pattern.cast::<IUIAutomationTogglePattern>())
        .and_then(|pattern| pattern.Toggle());
    std::mem::forget(element);
    result.map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("the exact UIA Toggle action failed: {error}"),
        )
    })
}

fn with_cleanup_scope<T, S, R, E>(
    value: T,
    scope: impl FnOnce() -> Result<S, E>,
    cleanup: impl FnOnce(&mut T) -> R,
) -> Result<R, E> {
    let _scope = scope()?;
    // Declare the owned value after the guard so unwinding preserves this order too.
    let mut value = value;
    let result = cleanup(&mut value);
    drop(value);
    Ok(result)
}

pub struct SetupUiHandle {
    hwnd: u64,
    window_identity: SetupWindowIdentity,
    owned_tab: Option<OwnedSetupTab>,
    new_tab_action_attempted: bool,
    descriptor: &'static BrowserSetupDescriptor,
    pub opened_setup_page: bool,
    pub enabled_remote_debugging: bool,
    pub focused_setup_address_field: bool,
    pub foregrounded_window: bool,
    pub injected_global_input: bool,
    enable_attempted: bool,
    initial_debugging_state: Option<CheckboxState>,
}

impl SetupUiHandle {
    fn revalidate_rollback_context(&self) -> Result<(), BrowserRefusal> {
        revalidate_rollback_context(
            || self.window_identity.revalidate(),
            self.owned_tab.as_ref(),
            |owned| owned.active(self.window_identity),
        )
    }
    fn rollback_remote_debugging(&mut self) -> bool {
        if !needs_debugging_restore(
            self.enabled_remote_debugging,
            self.enable_attempted,
            self.initial_debugging_state,
        ) {
            return true;
        }
        let Ok(_com) = SetupComScope::new() else {
            return false;
        };
        if self.window_identity.revalidate().is_err() {
            return false;
        }
        if self.initial_debugging_state != Some(CheckboxState::Off) {
            return false;
        }
        if self.revalidate_rollback_context().is_err() {
            return false;
        }
        let tree = crate::uia::walk_tree(self.hwnd, None);
        let checkbox = exact_setup_checkbox(&tree.nodes, self.descriptor);
        let observed = match checkbox {
            Ok(Some(element)) => unsafe {
                if self.revalidate_rollback_context().is_err() {
                    None
                } else {
                    match checkbox_state(element) {
                        Ok(CheckboxState::Off) => Some(true),
                        Ok(CheckboxState::On) if toggle(element).is_ok() => Some(false),
                        _ => None,
                    }
                }
            },
            _ => None,
        };
        release_nodes(&tree.nodes);
        if observed == Some(true) {
            if self.revalidate_rollback_context().is_err() {
                return false;
            }
            self.enabled_remote_debugging = false;
            return true;
        }
        if observed.is_none() || self.revalidate_rollback_context().is_err() {
            return false;
        }
        let readback = crate::uia::walk_tree(self.hwnd, None);
        let restored =
            exact_setup_checkbox(&readback.nodes, self.descriptor).is_ok_and(|checkbox| {
                checkbox.is_some_and(|element| unsafe {
                    matches!(checkbox_state(element), Ok(CheckboxState::Off))
                })
            }) && self.revalidate_rollback_context().is_ok();
        release_nodes(&readback.nodes);
        if restored {
            self.enabled_remote_debugging = false;
        }
        restored
    }

    pub fn abort(self, error: BrowserRefusal) -> BrowserRefusal {
        let mut original = error.clone();
        with_cleanup_scope(self, SetupComScope::new, |handle| {
            handle.abort_with_cleanup(error, None)
        })
        .unwrap_or_else(|cleanup_error| {
            let cause = original.detail.take();
            original.with_detail(serde_json::json!({
                "cause": cause,
                "setup_cleanup_refusal": cleanup_error.message,
            }))
        })
    }

    fn abort_with_cleanup(
        &mut self,
        error: BrowserRefusal,
        cleanup: Option<Result<Option<bool>, BrowserRefusal>>,
    ) -> BrowserRefusal {
        let enabled_remote_debugging = self.enabled_remote_debugging;
        let restored_remote_debugging = self.rollback_remote_debugging();
        let opened_setup_page = self.opened_setup_page;
        let focused_setup_address_field = self.focused_setup_address_field;
        let foregrounded_window = self.foregrounded_window;
        let injected_global_input = self.injected_global_input;
        let cleanup = cleanup.unwrap_or_else(|| self.close_owned_tab());
        let closed_setup_page = matches!(cleanup, Ok(Some(true)));
        let cleanup_status = match &cleanup {
            Ok(Some(true)) => "verified_closed",
            Ok(Some(false)) => "unconfirmed",
            Ok(None) if self.new_tab_action_attempted => "creation_unproven",
            Ok(None) => "not_opened",
            Err(_) => "refused",
        };
        let cleanup_refusal = cleanup.err().map(|error| error.message);
        let mut error = error;
        let cause = error.detail.take();
        error.with_detail(serde_json::json!({
            "setup_side_effects": {
                "opened_setup_page": opened_setup_page,
                "closed_setup_page": closed_setup_page,
                "focused_setup_address_field": focused_setup_address_field,
                "enabled_remote_debugging": enabled_remote_debugging,
                "foregrounded_window": foregrounded_window,
                "injected_global_input": injected_global_input,
                "restored_remote_debugging": restored_remote_debugging,
                "remote_debugging_restore_unconfirmed": self.enable_attempted && self.initial_debugging_state == Some(CheckboxState::Off) && !restored_remote_debugging,
                "owned_setup_tab_identified": self.owned_tab.is_some(),
                "setup_tab_creation_unproven": self.new_tab_action_attempted && !opened_setup_page && self.owned_tab.is_none(),
                "setup_tab_cleanup_status": cleanup_status,
                "setup_tab_cleanup_refusal": cleanup_refusal,
            },
            "cause": cause,
        }))
    }

    pub fn close_for_success(self) -> Result<Option<bool>, BrowserRefusal> {
        with_cleanup_scope(self, SetupComScope::new, |handle| {
            match handle.close_owned_tab() {
                Ok(Some(false)) => Err(handle.abort_with_cleanup(
                    refusal(
                        BrowserRefusalCode::BrowserBindingStale,
                        "the owned native setup tab did not disappear in a complete readback",
                    ),
                    Some(Ok(Some(false))),
                )),
                Err(error) => {
                    let cleanup_error = error.clone();
                    Err(handle.abort_with_cleanup(error, Some(Err(cleanup_error))))
                }
                result => result,
            }
        })?
    }

    fn close_owned_tab(&mut self) -> Result<Option<bool>, BrowserRefusal> {
        if !self.opened_setup_page {
            return Ok(None);
        }
        let owned =
            self.owned_tab.as_ref().ok_or_else(|| {
                refusal(BrowserRefusalCode::BrowserBindingAmbiguous,
            "the newly opened setup tab has no independently retained native ownership proof")
            })?;
        let closed = owned.close(self.window_identity)?;
        if closed {
            self.opened_setup_page = false;
        }
        Ok(Some(closed))
    }

    pub fn close(self) -> Option<bool> {
        with_cleanup_scope(self, SetupComScope::new, |handle| {
            handle.close_owned_tab().unwrap_or(Some(false))
        })
        .unwrap_or(Some(false))
    }
}

fn pending_setups() -> &'static Mutex<HashMap<u64, SetupUiHandle>> {
    static PENDING: OnceLock<Mutex<HashMap<u64, SetupUiHandle>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn retain_pending(hwnd: u64, handle: SetupUiHandle) -> Result<(), BrowserRefusal> {
    let mut pending = pending_setups().lock().unwrap();
    if pending.contains_key(&hwnd) {
        drop(pending);
        return Err(handle.abort(refusal(
            BrowserRefusalCode::BrowserBindingAmbiguous,
            "another approved browser setup is already pending for this exact window",
        )));
    }
    pending.insert(hwnd, handle);
    Ok(())
}

pub fn commit_pending(hwnd: u64) -> Result<bool, BrowserRefusal> {
    let handle = pending_setups()
        .lock()
        .unwrap()
        .remove(&hwnd)
        .ok_or_else(|| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the exact pending browser setup cleanup handle is missing",
            )
        })?;
    Ok(handle.close_for_success()?.unwrap_or(false))
}

pub fn abort_pending(hwnd: u64, error: BrowserRefusal) -> BrowserRefusal {
    match pending_setups().lock().unwrap().remove(&hwnd) {
        Some(handle) => handle.abort(error),
        None => error.with_detail(serde_json::json!({
            "setup_cleanup": "the exact pending browser setup cleanup handle was missing"
        })),
    }
}

fn set_remote_debugging(
    hwnd: u64,
    descriptor: &'static BrowserSetupDescriptor,
    desired_enabled: bool,
) -> Result<SetupUiHandle, BrowserRefusal> {
    let _com = SetupComScope::new()?;
    let window_identity = SetupWindowIdentity::capture(hwnd)?;
    let initial_native = native_nodes(hwnd)?;
    let setup_page_active = needs_setup_page_renderer(&initial_native, descriptor);
    release_nodes(&initial_native);
    // Only the setup page's own checkbox needs renderer-owned nodes. Any other
    // active page is left unread, so an unresponsive renderer cannot stall
    // setup or cleanup before the dedicated setup tab exists.
    let initial = if setup_page_active {
        crate::uia::walk_tree(hwnd, None).nodes
    } else {
        Vec::new()
    };
    let initial_checkbox = exact_setup_checkbox(&initial, descriptor);
    let mut handle = match initial_checkbox {
        Ok(Some(_)) => SetupUiHandle {
            hwnd,
            window_identity,
            owned_tab: None,
            new_tab_action_attempted: false,
            descriptor,
            opened_setup_page: false,
            enabled_remote_debugging: false,
            focused_setup_address_field: false,
            foregrounded_window: false,
            injected_global_input: false,
            enable_attempted: false,
            initial_debugging_state: None,
        },
        Ok(None) => {
            release_nodes(&initial);

            let mut handle = SetupUiHandle {
                hwnd,
                window_identity,
                owned_tab: None,
                new_tab_action_attempted: false,
                descriptor,
                opened_setup_page: false,
                enabled_remote_debugging: false,
                focused_setup_address_field: false,
                foregrounded_window: false,
                injected_global_input: false,
                enable_attempted: false,
                initial_debugging_state: None,
            };
            let tab_tree = match native_nodes(hwnd) {
                Ok(nodes) => nodes,
                Err(error) => return Err(handle.abort(error)),
            };
            let before = (|| {
                window_identity.revalidate()?;
                let tabs = native_tab_identities(&tab_tree, runtime_id)?;
                if tabs.is_empty() {
                    return Err(refusal(
                        BrowserRefusalCode::BrowserBindingAmbiguous,
                        "the approved browser had no complete native tab identity baseline",
                    ));
                }
                let window = OwnedNativeElement::capture(native_window_node(&tab_tree)?)?;
                let originals = tabs
                    .iter()
                    .map(|tab| OwnedNativeElement::capture(&tab_tree[tab.node_index]))
                    .collect::<Result<Vec<_>, _>>()?;
                prove_native_baseline(
                    &window,
                    &originals,
                    &tabs,
                    &tabs,
                    &tab_tree,
                    window_identity,
                )?;
                Ok((tabs, originals, window))
            })();
            let (before, original_tabs, owned_window) = match before {
                Ok(value) => value,
                Err(error) => {
                    release_nodes(&tab_tree);
                    return Err(handle.abort(error));
                }
            };
            let new_tab_button = match exact_native_new_tab_button(&tab_tree) {
                Ok(Some(element)) => element,
                Ok(None) => {
                    release_nodes(&tab_tree);
                    return Err(handle.abort(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "the exact {} window has no structural native new-tab action",
                            descriptor.product_name
                        ),
                    )));
                }
                Err(error) => {
                    release_nodes(&tab_tree);
                    return Err(handle.abort(error));
                }
            };
            if let Err(error) = window_identity.revalidate() {
                release_nodes(&tab_tree);
                return Err(handle.abort(error));
            }
            handle.new_tab_action_attempted = true;
            let invocation = unsafe { invoke(new_tab_button, "native new-tab button") };
            release_nodes(&tab_tree);
            // A failed Invoke can have an unknown side effect. It cannot
            // establish causality for an unrelated concurrently created tab.
            if let Err(error) = invocation {
                return Err(handle.abort(error));
            }
            handle.opened_setup_page = true;

            let deadline = Instant::now() + Duration::from_secs(3);
            let mut created = loop {
                let tree = match native_nodes(hwnd) {
                    Ok(tree) => tree,
                    Err(error) => return Err(handle.abort(error)),
                };
                let selected = native_tab_identities(&tree, runtime_id).and_then(|after| {
                    prove_native_baseline(
                        &owned_window,
                        &original_tabs,
                        &before,
                        &after,
                        &tree,
                        window_identity,
                    )
                });
                match selected {
                    Ok(Some(index)) => {
                        let owned = OwnedSetupTab::capture(&tree, index, owned_window);
                        match owned {
                            Ok(owned) => {
                                handle.owned_tab = Some(owned);
                                handle.opened_setup_page = true;
                                break tree;
                            }
                            Err(error) => {
                                release_nodes(&tree);
                                return Err(handle.abort(error));
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        release_nodes(&tree);
                        return Err(handle.abort(error));
                    }
                }
                release_nodes(&tree);
                if Instant::now() >= deadline {
                    let error = refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "{} did not expose exactly one newly created tab",
                            descriptor.product_name
                        ),
                    );
                    return Err(handle.abort(error));
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            if let Err(error) = window_identity.revalidate() {
                release_nodes(&created);
                return Err(handle.abort(error));
            }
            let omnibox = match unique_native_actionable(&created, "Edit", "set_value") {
                Ok(Some(element)) => element,
                Ok(None) => {
                    release_nodes(&created);
                    return Err(handle.abort(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "the approved {} window has no unique native editable address field",
                            descriptor.product_name
                        ),
                    )));
                }
                Err(error) => {
                    release_nodes(&created);
                    return Err(handle.abort(error));
                }
            };
            if let Err(error) = unsafe { set_value(omnibox, descriptor.setup_url) } {
                release_nodes(&created);
                return Err(handle.abort(error));
            }
            release_nodes(&created);
            created = match native_nodes(hwnd) {
                Ok(nodes) => nodes,
                Err(error) => return Err(handle.abort(error)),
            };
            let refreshed_omnibox = match unique_native_actionable(&created, "Edit", "set_value") {
                Ok(Some(element))
                    if created.iter().any(|node| {
                        node.element_ptr == element
                            && node.value.as_deref().is_some_and(|value| {
                                value.trim().eq_ignore_ascii_case(descriptor.setup_url)
                            })
                    }) =>
                {
                    element
                }
                Ok(_) => {
                    release_nodes(&created);
                    return Err(handle.abort(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        "the unique native address field did not retain the exact setup URL",
                    )));
                }
                Err(error) => {
                    release_nodes(&created);
                    return Err(handle.abort(error));
                }
            };
            if let Err(error) = confirm_setup_navigation(
                hwnd,
                refreshed_omnibox,
                &mut handle.foregrounded_window,
                &mut handle.injected_global_input,
                &mut handle.focused_setup_address_field,
            ) {
                release_nodes(&created);
                return Err(handle.abort(error));
            }
            release_nodes(&created);
            handle
        }
        Err(error) => {
            release_nodes(&initial);
            return Err(error);
        }
    };
    if !handle.opened_setup_page {
        release_nodes(&initial);
    }

    let deadline = Instant::now() + EXISTING_PROFILE_SETUP_READY_TIMEOUT;
    loop {
        if let Err(error) = window_identity.revalidate() {
            return Err(handle.abort(error));
        }
        let tree = crate::uia::walk_tree(hwnd, None);
        let checkbox = exact_setup_checkbox(&tree.nodes, descriptor);
        match checkbox {
            Ok(Some(element)) => {
                let state = unsafe { checkbox_state(element) };
                if let Ok(state) = &state {
                    handle.initial_debugging_state.get_or_insert(*state);
                }
                let outcome = match state {
                    Ok(state) if (state == CheckboxState::On) == desired_enabled => {
                        if desired_enabled && handle.enable_attempted {
                            handle.enabled_remote_debugging = true;
                        } else if !desired_enabled {
                            handle.enabled_remote_debugging = false;
                        }
                        Ok(true)
                    }
                    Ok(_) if !handle.enable_attempted => {
                        attempt_setup_toggle(&mut handle.enable_attempted, || unsafe {
                            toggle(element)
                        })
                    }
                    Ok(_) => Ok(false),
                    Err(error) => Err(error),
                };
                release_nodes(&tree.nodes);
                match outcome {
                    Ok(true) => return Ok(handle),
                    Ok(false) => {}
                    Err(error) => return Err(handle.abort(error)),
                }
            }
            Ok(None) => release_nodes(&tree.nodes),
            Err(error) => {
                release_nodes(&tree.nodes);
                return Err(handle.abort(error));
            }
        }
        if Instant::now() >= deadline {
            return Err(handle.abort(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!(
                    "the exact {} remote-debugging setup page did not become ready",
                    descriptor.product_name
                ),
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn enable(
    hwnd: u64,
    descriptor: &'static BrowserSetupDescriptor,
) -> Result<SetupUiHandle, BrowserRefusal> {
    set_remote_debugging(hwnd, descriptor, true)
}

pub fn disable(
    hwnd: u64,
    descriptor: &'static BrowserSetupDescriptor,
) -> Result<bool, BrowserRefusal> {
    let handle = set_remote_debugging(hwnd, descriptor, false)?;
    Ok(handle.close().unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::browser::{existing_profile_setup_descriptor, BrowserProduct};

    #[test]
    fn owned_cleanup_values_drop_before_scope_on_success_error_and_unwind() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Tracked(&'static str, Rc<RefCell<Vec<&'static str>>>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }
        for mode in 0..3 {
            let events = Rc::new(RefCell::new(Vec::new()));
            let owned = Tracked("owned", events.clone());
            let scope_events = events.clone();
            let cleanup_events = events.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_cleanup_scope(
                    owned,
                    || Ok::<_, ()>(Tracked("scope", scope_events)),
                    |_| {
                        cleanup_events.borrow_mut().push("cleanup");
                        match mode {
                            0 => Ok(()),
                            1 => Err(()),
                            _ => panic!("simulated cleanup unwind"),
                        }
                    },
                )
            }));
            match mode {
                0 => assert_eq!(outcome.unwrap(), Ok(Ok(()))),
                1 => assert_eq!(outcome.unwrap(), Ok(Err(()))),
                _ => assert!(outcome.is_err()),
            }
            assert_eq!(*events.borrow(), ["cleanup", "owned", "scope"]);
        }
    }
    fn descriptor() -> &'static BrowserSetupDescriptor {
        existing_profile_setup_descriptor(BrowserProduct::GoogleChrome).unwrap()
    }

    #[test]
    fn existing_setup_page_rollback_requires_window_identity_without_an_owned_tab() {
        let checks = std::cell::Cell::new(0);
        assert!(revalidate_rollback_context(
            || {
                checks.set(checks.get() + 1);
                Ok(())
            },
            None::<&()>,
            |_| panic!("an existing page has no driver-created tab"),
        )
        .is_ok());
        assert_eq!(checks.get(), 1);
        assert!(revalidate_rollback_context(
            || Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "window changed"
            )),
            None::<&()>,
            |_| panic!("window failure must stop tab validation"),
        )
        .is_err());
    }

    #[test]
    fn created_setup_tab_rollback_still_requires_live_owned_identity() {
        let checks = std::cell::Cell::new(0);
        assert!(revalidate_rollback_context(
            || {
                checks.set(1);
                Ok(())
            },
            Some(&()),
            |_| {
                assert_eq!(checks.get(), 1);
                checks.set(2);
                Ok(())
            },
        )
        .is_ok());
        assert_eq!(checks.get(), 2);
        assert!(revalidate_rollback_context(
            || Ok(()),
            Some(&()),
            |_| Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "owned tab changed"
            )),
        )
        .is_err());
    }
    #[test]
    fn failed_toggle_that_changed_state_still_requires_proven_rollback() {
        let mut attempted = false;
        let actual = std::cell::Cell::new(CheckboxState::Off);
        let result = attempt_setup_toggle(&mut attempted, || {
            actual.set(CheckboxState::On);
            Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "Toggle changed state but returned failure",
            ))
        });
        assert!(result.is_err());
        assert_eq!(actual.get(), CheckboxState::On);
        assert!(attempted);
        assert!(needs_debugging_restore(
            false,
            attempted,
            Some(CheckboxState::Off)
        ));
        assert!(!needs_debugging_restore(
            false,
            false,
            Some(CheckboxState::Off)
        ));
    }

    fn owned_fixture(ids: &[i32]) -> Vec<UiaNode> {
        let mut rows = Vec::new();
        for id in ids {
            let mut tab = node("TabItem", "opaque tab", None, &["select"]);
            tab.element_ptr = *id as usize;
            tab.depth = 2;
            tab.enabled = Some(true);
            rows.push(tab);
            let mut close = node("Button", "opaque action", None, &["invoke"]);
            close.element_ptr = (*id + 100) as usize;
            close.depth = 3;
            close.enabled = Some(true);
            rows.push(close);
        }
        rows
    }

    fn fixture_id(ptr: usize) -> Result<RuntimeId, BrowserRefusal> {
        RuntimeId::new(vec![42, ptr as i32])
    }

    fn fixture_tabs(ids: &[i32]) -> Vec<NativeTab> {
        native_tab_identities(&owned_fixture(ids), fixture_id).unwrap()
    }

    fn owned_proof() -> OwnedTabProof {
        OwnedTabProof {
            tab: fixture_id(3).unwrap(),
            close: fixture_id(103).unwrap(),
        }
    }

    #[test]
    fn newly_owned_tab_is_the_unique_complete_identity_difference() {
        let before = fixture_tabs(&[1, 2]);
        let after = fixture_tabs(&[2, 3, 1]);
        assert_eq!(select_new_native_tab(&before, &after).unwrap(), Some(2));
        assert!(select_new_native_tab(&before, &fixture_tabs(&[1, 2]))
            .unwrap()
            .is_none());
        assert!(select_new_native_tab(&before, &fixture_tabs(&[1, 2, 3, 4])).is_err());
        assert!(select_new_native_tab(&before, &fixture_tabs(&[2, 3])).is_err());
    }

    #[test]
    fn reused_baseline_id_cannot_turn_a_foreign_new_tab_into_owned_cleanup() {
        let before = fixture_tabs(&[1, 2]);
        // Old tab 1 died; our new tab reused its ID, while a foreign tab 3
        // appeared. Opaque ID subtraction alone would incorrectly choose 3.
        let after = fixture_tabs(&[1, 2, 3]);
        assert_eq!(select_new_native_tab(&before, &after).unwrap(), Some(4));
        let proven = select_new_native_tab_with_baseline(&before, &after, |old, _| {
            if old.id == fixture_id(1).unwrap() {
                Err(refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    "the retained original object is unavailable or different",
                ))
            } else {
                Ok(())
            }
        });
        let invoked = std::cell::Cell::new(0);
        assert!(invoke_owned_close_once(
            proven,
            |_| {
                invoked.set(invoked.get() + 1);
                Ok(())
            },
            |_| Ok(true)
        )
        .is_err());
        assert_eq!(invoked.get(), 0);
    }

    #[test]
    fn duplicate_foreign_close_identity_refuses_cleanup() {
        let mut rows = owned_fixture(&[1, 2, 3]);
        rows[1].element_ptr = 103;
        assert!(owned_proof().close_button(&rows, fixture_id).is_err());
    }

    #[test]
    fn inert_native_tab_is_still_present_and_preserves_subtree_boundaries() {
        let mut rows = owned_fixture(&[1, 2, 3]);
        rows[4].name = None;
        rows[4].actions.clear();
        let after = native_tab_identities(&rows, fixture_id).unwrap();
        assert!(!owned_tab_disappeared(
            &owned_proof(),
            &fixture_tabs(&[1, 2, 3]),
            &after
        ));
        let mut rows = owned_fixture(&[3, 4]);
        rows.remove(1); // The owned tab has no close; foreign tab 4 does.
        rows[1].name = None;
        rows[1].actions.clear();
        assert!(owned_proof().close_button(&rows, fixture_id).is_err());
    }

    #[test]
    fn tab_identity_requires_every_native_runtime_id_and_rejects_duplicates() {
        let rows = owned_fixture(&[1, 2]);
        assert!(native_tab_identities(&rows, |_| RuntimeId::new(Vec::new())).is_err());
        assert!(native_tab_identities(&rows, |_| RuntimeId::new(vec![1])).is_err());
        assert!(native_tab_identities(&rows, |_| Err(refusal(
            BrowserRefusalCode::BrowserBindingStale,
            "missing runtime ID"
        )))
        .is_err());
        assert!(RuntimeId::new(vec![0; MAX_RUNTIME_ID_PARTS + 1]).is_err());
    }

    #[test]
    fn cleanup_selects_only_owned_native_tab_among_foreign_tabs() {
        let rows = owned_fixture(&[4, 2, 3, 1, 5]);
        assert_eq!(owned_proof().close_button(&rows, fixture_id).unwrap(), 5);
        let mut renderer_spoof = owned_fixture(&[3]);
        for row in &mut renderer_spoof {
            row.in_web_content = true;
        }
        assert!(owned_proof()
            .close_button(&renderer_spoof, fixture_id)
            .is_err());
    }

    #[test]
    fn cleanup_refuses_runtime_id_drift_or_replacement_close_button() {
        assert!(owned_proof()
            .close_button(&owned_fixture(&[1, 2, 4]), fixture_id)
            .is_err());
        let mut rows = owned_fixture(&[1, 2, 3]);
        rows[5].element_ptr = 999;
        assert!(owned_proof().close_button(&rows, fixture_id).is_err());
    }

    #[test]
    fn cleanup_refuses_missing_or_ambiguous_native_close_action() {
        let mut rows = owned_fixture(&[3]);
        rows[1].actions.clear();
        assert!(owned_proof().close_button(&rows, fixture_id).is_err());
        rows[1].actions.push("invoke".to_owned());
        let mut other = rows[1].clone();
        other.element_ptr = 104;
        rows.push(other);
        assert!(owned_proof().close_button(&rows, fixture_id).is_err());
    }

    #[test]
    fn renderer_navigation_failure_does_not_prevent_owned_native_cleanup() {
        let rows = owned_fixture(&[1, 2, 3]);
        assert!(!setup_page_proven(&rows, descriptor()));
        assert_eq!(owned_proof().close_button(&rows, fixture_id).unwrap(), 5);
        assert!(owned_tab_disappeared(
            &owned_proof(),
            &fixture_tabs(&[1, 2, 3]),
            &fixture_tabs(&[1, 2])
        ));
    }

    #[test]
    fn a_committed_setup_url_without_owned_identity_never_authorizes_cleanup() {
        let rows = vec![
            node(
                "Edit",
                "address",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", "setup", None, &[]),
        ];
        assert!(setup_page_proven(&rows, descriptor()));
        assert!(owned_proof().close_button(&rows, fixture_id).is_err());
    }

    #[test]
    fn closed_requires_complete_readback_with_only_owned_tab_removed() {
        let before = fixture_tabs(&[1, 2, 3]);
        assert!(!owned_tab_disappeared(&owned_proof(), &before, &before));
        assert!(!owned_tab_disappeared(
            &owned_proof(),
            &before,
            &fixture_tabs(&[2])
        ));
        assert!(owned_tab_disappeared(
            &owned_proof(),
            &before,
            &fixture_tabs(&[2, 1, 4])
        ));
    }

    #[test]
    fn stale_or_unavailable_ownership_causes_zero_close_input() {
        let invoked = std::cell::Cell::new(0);
        let read = std::cell::Cell::new(0);
        for message in [
            "PID birth changed",
            "retained element unavailable",
            "COM resolve failed",
            "partial native tree",
            "ambiguous tab IDs",
        ] {
            let result = invoke_owned_close_once::<()>(
                Err(refusal(BrowserRefusalCode::BrowserBindingStale, message)),
                |_| {
                    invoked.set(invoked.get() + 1);
                    Ok(())
                },
                |_| {
                    read.set(read.get() + 1);
                    Ok(true)
                },
            );
            assert!(result.is_err());
        }
        assert_eq!(invoked.get(), 0);
        assert_eq!(read.get(), 0);
    }

    #[test]
    fn native_invoke_success_without_readback_never_claims_closed_or_reinvokes() {
        for outcome in [
            Ok(false),
            Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "incomplete readback",
            )),
        ] {
            let invoked = std::cell::Cell::new(0);
            let result = invoke_owned_close_once(
                Ok(()),
                |_| {
                    invoked.set(invoked.get() + 1);
                    Ok(())
                },
                |_| outcome,
            );
            assert!(!matches!(result, Ok(true)));
            assert_eq!(invoked.get(), 1);
        }
    }

    #[test]
    fn invoke_failure_does_not_retry_or_claim_a_readback() {
        let invoked = std::cell::Cell::new(0);
        let read = std::cell::Cell::new(0);
        let result = invoke_owned_close_once(
            Ok(()),
            |_| {
                invoked.set(invoked.get() + 1);
                Err(refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    "invoke failed",
                ))
            },
            |_| {
                read.set(read.get() + 1);
                Ok(true)
            },
        );
        assert!(result.is_err());
        assert_eq!(invoked.get(), 1);
        assert_eq!(read.get(), 0);
    }

    #[test]
    fn navigation_failure_cleanup_invokes_only_the_proven_owned_close_action() {
        let rows = owned_fixture(&[1, 2, 3, 4]);
        assert!(!setup_page_proven(&rows, descriptor()));
        let proven = owned_proof().close_button(&rows, fixture_id);
        let invoked = std::cell::Cell::new(None);
        let result = invoke_owned_close_once(
            proven,
            |index| {
                invoked.set(Some(rows[*index].element_ptr));
                Ok(())
            },
            |_| {
                Ok(owned_tab_disappeared(
                    &owned_proof(),
                    &fixture_tabs(&[1, 2, 3, 4]),
                    &fixture_tabs(&[1, 2, 4]),
                ))
            },
        );
        assert!(result.unwrap());
        assert_eq!(invoked.get(), Some(103));
    }

    fn node(control_type: &str, name: &str, value: Option<&str>, actions: &[&str]) -> UiaNode {
        UiaNode {
            element_index: (!actions.is_empty()).then_some(0),
            control_type: control_type.to_owned(),
            name: Some(name.to_owned()),
            value: value.map(str::to_owned),
            automation_id: None,
            help_text: None,
            actions: actions.iter().map(|value| (*value).to_owned()).collect(),
            enabled: None,
            selected: None,
            element_ptr: 7,
            center_x: 0,
            center_y: 0,
            rect: None,
            msaa_role: None,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    #[test]
    fn checkbox_requires_exact_internal_url_and_unique_web_toggle() {
        let mut checkbox = node("CheckBox", descriptor().checkbox_label, None, &["toggle"]);
        checkbox.in_web_content = true;
        let nodes = vec![
            node(
                "Edit",
                "Address and search bar",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", descriptor().page_titles[0], None, &[]),
            node("Header", descriptor().page_heading, None, &[]),
            checkbox,
        ];
        assert_eq!(exact_setup_checkbox(&nodes, descriptor()).unwrap(), Some(7));

        let mut localized = nodes.clone();
        localized[0].name = Some("アドレス検索バー".to_owned());
        localized[1].name = Some("远程调试页面".to_owned());
        localized[2].name = Some("Удалённая отладка".to_owned());
        localized[3].name = Some("السماح بتصحيح الأخطاء لهذا المتصفح".to_owned());
        localized[3].in_web_content = true;
        assert_eq!(
            exact_setup_checkbox(&localized, descriptor()).unwrap(),
            Some(7)
        );

        let mut wrong_url = nodes.clone();
        wrong_url[0].value = Some("https://example.test/".to_owned());
        assert_eq!(
            exact_setup_checkbox(&wrong_url, descriptor()).unwrap(),
            None
        );
    }

    #[test]
    fn setup_page_names_are_opaque_across_unicode_scripts_and_normalization() {
        let samples = [
            ("e\u{301}", "é", "✅"),
            ("हिन्दी", "ไทย", "עברית"),
            ("日本語", "한국어", "简体中文"),
            ("\u{2067}العربية\u{2069}", "فارسی", "اردو"),
            ("Հայերեն", "ქართული", "አማርኛ"),
            ("👩🏽‍💻", "A\u{200d}B", "𐐷"),
            ("", "", ""),
        ];
        for (document_name, heading_name, checkbox_name) in samples {
            let mut checkbox = node("CheckBox", checkbox_name, None, &["toggle"]);
            checkbox.in_web_content = true;
            let nodes = vec![
                node(
                    "Edit",
                    "opaque native address field",
                    Some(descriptor().setup_url),
                    &["set_value"],
                ),
                node("Document", document_name, None, &[]),
                node("Header", heading_name, None, &[]),
                checkbox,
            ];
            assert_eq!(exact_setup_checkbox(&nodes, descriptor()).unwrap(), Some(7));
        }
    }

    #[test]
    fn setup_page_does_not_require_accessible_names() {
        let mut checkbox = node("CheckBox", "placeholder", None, &["toggle"]);
        checkbox.name = None;
        checkbox.in_web_content = true;
        let mut address = node(
            "Edit",
            "placeholder",
            Some(descriptor().setup_url),
            &["set_value"],
        );
        address.name = None;
        let mut document = node("Document", "placeholder", None, &[]);
        document.name = None;
        let nodes = vec![address, document, checkbox];

        assert_eq!(exact_setup_checkbox(&nodes, descriptor()).unwrap(), Some(7));
    }

    #[test]
    fn setup_page_refuses_ambiguous_web_toggles_without_reading_their_names() {
        let mut first = node("CheckBox", "A", None, &["toggle"]);
        first.element_ptr = 41;
        first.in_web_content = true;
        let mut second = node("CheckBox", "B", None, &["toggle"]);
        second.element_ptr = 42;
        second.in_web_content = true;
        let nodes = vec![
            node(
                "Edit",
                "address",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", "opaque", None, &[]),
            first,
            second,
        ];

        assert_eq!(
            exact_setup_checkbox(&nodes, descriptor()).unwrap_err().code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
    }

    #[test]
    fn native_address_control_is_language_independent_without_trusting_web_content() {
        let mut native_address = node(
            "Edit",
            "アドレス検索バー",
            Some("chrome://inspect/#remote-debugging"),
            &["set_value"],
        );
        native_address.element_ptr = 11;
        let mut renderer_spoof = node(
            "Edit",
            "アドレス検索バー",
            Some("chrome://inspect/#remote-debugging"),
            &["set_value"],
        );
        renderer_spoof.element_ptr = 12;
        renderer_spoof.in_web_content = true;

        assert_eq!(
            unique_native_actionable(&[native_address, renderer_spoof], "Edit", "set_value")
                .unwrap(),
            Some(11)
        );
    }

    #[test]
    fn native_browser_controls_still_refuse_ambiguous_structural_matches() {
        let mut first = node("Edit", "Adress- und Suchleiste", None, &["set_value"]);
        first.element_ptr = 31;
        let mut second = node(
            "Edit",
            "Barre d'adresse et de recherche",
            None,
            &["set_value"],
        );
        second.element_ptr = 32;

        let error =
            unique_native_actionable_with_focus(&[first, second], "Edit", "set_value", |_| false)
                .unwrap_err();
        assert_eq!(error.code, BrowserRefusalCode::BrowserWrongTargetRefused);
    }

    #[test]
    fn native_address_control_uses_unique_keyboard_focus_when_edge_exposes_multiple_edits() {
        let mut first = node("Edit", "opaque first", None, &["set_value"]);
        first.element_ptr = 31;
        let mut second = node("Edit", "opaque second", None, &["set_value"]);
        second.element_ptr = 32;

        assert_eq!(
            unique_native_actionable_with_focus(&[first, second], "Edit", "set_value", |element| {
                element == 32
            },)
            .unwrap(),
            Some(32)
        );
    }

    #[test]
    fn native_address_control_refuses_multiple_focused_edits() {
        let mut first = node("Edit", "opaque first", None, &["set_value"]);
        first.element_ptr = 31;
        let mut second = node("Edit", "opaque second", None, &["set_value"]);
        second.element_ptr = 32;

        assert_eq!(
            unique_native_actionable_with_focus(&[first, second], "Edit", "set_value", |_| true,)
                .unwrap_err()
                .code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
    }

    #[test]
    fn setup_page_proof_rejects_duplicate_native_exact_urls() {
        let mut nodes = vec![
            node(
                "Edit",
                "Barra de direcciones y de búsqueda",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", descriptor().page_titles[0], None, &[]),
            node("Header", descriptor().page_heading, None, &[]),
        ];
        let mut duplicate = nodes[0].clone();
        duplicate.element_ptr = 99;
        nodes.push(duplicate);

        assert!(!setup_page_proven(&nodes, descriptor()));
    }

    #[test]
    fn native_tab_count_is_structural_and_deduplicates_repeated_uia_rows() {
        let mut first = node("TabItem", "新标签页", None, &[]);
        first.rect = Some((10, 10, 110, 40));
        let duplicate = first.clone();
        let mut second = node("TabItem", "Neue Registerkarte", None, &[]);
        second.rect = Some((120, 10, 220, 40));
        let mut renderer_spoof = node("TabItem", "Tab", None, &[]);
        renderer_spoof.rect = Some((230, 10, 330, 40));
        renderer_spoof.in_web_content = true;
        let mut invalid = node("TabItem", "Onglet", None, &[]);
        invalid.rect = Some((0, 0, 0, 0));

        assert_eq!(
            native_tab_count(&[first, duplicate, second, renderer_spoof, invalid]),
            2
        );
    }

    #[test]
    fn native_new_tab_button_is_the_strict_successor_of_the_tab_strip() {
        let mut first = node("TabItem", "opaque-1", None, &["select"]);
        first.element_index = Some(10);
        first.element_ptr = 10;
        first.depth = 8;
        first.rect = Some((10, 10, 110, 40));
        let mut first_close = node("Button", "opaque-close-1", None, &["invoke"]);
        first_close.element_index = Some(11);
        first_close.element_ptr = 11;
        first_close.depth = 9;
        first_close.parent_element_index = Some(10);
        let mut second = node("TabItem", "opaque-2", None, &["select"]);
        second.element_index = Some(20);
        second.element_ptr = 20;
        second.depth = 8;
        second.rect = Some((120, 10, 220, 40));
        let mut second_close = node("Button", "opaque-close-2", None, &["invoke"]);
        second_close.element_index = Some(21);
        second_close.element_ptr = 21;
        second_close.depth = 9;
        second_close.parent_element_index = Some(20);
        let mut new_tab = node("Button", "opaque-new-tab", None, &["invoke"]);
        new_tab.element_ptr = 30;
        new_tab.depth = 6;
        new_tab.rect = Some((220, 10, 260, 40));

        assert_eq!(
            exact_native_new_tab_button(&[first, first_close, second, second_close, new_tab,])
                .unwrap(),
            Some(30)
        );
    }

    #[test]
    fn strict_identity_rows_do_not_change_the_new_tab_action_projection() {
        let mut tab = node("TabItem", "tab", None, &["select"]);
        tab.depth = 2;
        tab.rect = Some((10, 10, 110, 40));
        tab.element_ptr = 10;
        let mut close = node("Button", "close", None, &["invoke"]);
        close.depth = 3;
        close.element_ptr = 11;
        let mut pane = node("Pane", "", None, &[]);
        pane.name = None;
        pane.depth = 2;
        pane.element_ptr = 12;
        let mut new_tab = node("Button", "new tab", None, &["invoke"]);
        new_tab.depth = 2;
        new_tab.rect = Some((110, 10, 150, 40));
        new_tab.element_ptr = 13;
        let rows = vec![tab, close, pane, new_tab];
        assert_eq!(exact_native_new_tab_button(&rows).unwrap(), Some(13));
        // The inert Pane stays in the identity view: a duplicate ID on that
        // row must still refuse ownership even though the action view hides it.
        assert!(
            native_tab_identities(&rows, |ptr| fixture_id(if ptr == 12 { 10 } else { ptr }))
                .is_err()
        );
    }

    /// Native Edge 153.0.4234.48 chrome captured on hosted Windows while the
    /// fixture renderer stopped answering UIA during `end_session` (#4121).
    fn edge_153_native_chrome(url: &str, document: &str) -> Vec<UiaNode> {
        let mut rows = Vec::new();
        let mut push = |control_type: &str,
                        name: &str,
                        value: Option<&str>,
                        actions: &[&str],
                        depth: usize,
                        ptr: usize,
                        rect: Option<(i32, i32, i32, i32)>| {
            let mut row = node(control_type, name, value, actions);
            row.depth = depth;
            row.element_ptr = ptr;
            row.element_index = (!actions.is_empty()).then_some(ptr);
            row.rect = rect;
            rows.push(row);
        };
        push(
            "Window",
            "cua-driver Web Harness - Profile 1 - Microsoft Edge",
            None,
            &[],
            0,
            1,
            None,
        );
        push(
            "Pane",
            "cua-driver Web Harness - Microsoft Edge",
            None,
            &[],
            1,
            2,
            None,
        );
        push(
            "Button",
            "Minimize",
            None,
            &["invoke"],
            5,
            3,
            Some((794, 41, 840, 80)),
        );
        push(
            "Button",
            "Maximize",
            None,
            &["invoke"],
            5,
            4,
            Some((840, 41, 886, 80)),
        );
        push(
            "Button",
            "Close",
            None,
            &["invoke"],
            5,
            5,
            Some((886, 41, 932, 80)),
        );
        push("ToolBar", "App bar", None, &[], 6, 6, None);
        push(
            "Button",
            "Back",
            None,
            &["invoke"],
            7,
            7,
            Some((52, 84, 84, 116)),
        );
        push(
            "Button",
            "Refresh",
            None,
            &["invoke"],
            7,
            8,
            Some((88, 84, 120, 116)),
        );
        push(
            "Edit",
            "Address and search bar",
            Some(url),
            &["expand", "set_value", "text"],
            8,
            9,
            Some((152, 88, 689, 112)),
        );
        push(
            "Document",
            document,
            None,
            &["set_value", "text", "scroll"],
            7,
            10,
            Some((48, 127, 932, 672)),
        );
        push("Tab", "Tab bar", None, &[], 5, 11, None);
        push(
            "Button",
            "Search tabs",
            None,
            &["expand"],
            6,
            12,
            Some((52, 40, 84, 76)),
        );
        push(
            "TabItem",
            "cua-driver Web Harness - Memory usage - 26.6 MB",
            None,
            &["select"],
            8,
            13,
            Some((80, 40, 336, 81)),
        );
        push(
            "Button",
            "Close tab",
            None,
            &["invoke"],
            9,
            14,
            Some((304, 52, 320, 68)),
        );
        push(
            "Button",
            "New Tab",
            None,
            &["invoke"],
            6,
            15,
            Some((332, 40, 364, 81)),
        );
        rows
    }

    #[test]
    fn edge_153_cleanup_uses_native_chrome_without_reading_the_page_renderer() {
        let edge = existing_profile_setup_descriptor(BrowserProduct::MicrosoftEdge).unwrap();
        let fixture = edge_153_native_chrome("127.0.0.1:58963/fixture", "cua-driver Web Harness");
        assert!(
            !needs_setup_page_renderer(&fixture, edge),
            "an unrelated active page must not require renderer-owned nodes"
        );
        assert_eq!(native_tab_count(&fixture), 1);
        assert_eq!(exact_native_new_tab_button(&fixture).unwrap(), Some(15));
        assert_eq!(
            unique_native_actionable_with_focus(&fixture, "Edit", "set_value", |_| false).unwrap(),
            Some(9)
        );

        let setup = edge_153_native_chrome(edge.setup_url, edge.page_titles[0]);
        assert!(
            needs_setup_page_renderer(&setup, edge),
            "the committed setup page still requires its renderer-owned checkbox"
        );
    }
}
