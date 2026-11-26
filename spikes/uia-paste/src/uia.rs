use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VariantClear};
use windows::Win32::UI::Accessibility::*;
use windows::core::{Interface, Result};

use crate::policy::{Bounds, Node};

pub const LOOKUP_TIMEOUT: Duration = Duration::from_millis(250);

pub struct Apartment;

impl Apartment {
    pub fn init(model: COINIT) -> Result<Self> {
        unsafe { CoInitializeEx(None, model).ok()? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[derive(Default)]
pub struct Snapshot {
    pub chain: Vec<Node>,
    pub focus: Option<Node>,
    pub errors: Vec<String>,
}

pub struct Automation {
    uia: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    cache: IUIAutomationCacheRequest,
}

impl Automation {
    pub fn new() -> Result<Self> {
        unsafe {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            let uia2 = uia.cast::<IUIAutomation2>()?;
            uia2.SetConnectionTimeout(LOOKUP_TIMEOUT.as_millis() as u32)?;
            uia2.SetTransactionTimeout(LOOKUP_TIMEOUT.as_millis() as u32)?;
            let walker = uia.RawViewWalker()?;
            let cache = uia.CreateCacheRequest()?;
            for property in [
                UIA_ControlTypePropertyId,
                UIA_LocalizedControlTypePropertyId,
                UIA_ClassNamePropertyId,
                UIA_NamePropertyId,
                UIA_FrameworkIdPropertyId,
                UIA_IsKeyboardFocusablePropertyId,
                UIA_HasKeyboardFocusPropertyId,
                UIA_IsPasswordPropertyId,
                UIA_BoundingRectanglePropertyId,
                UIA_ProcessIdPropertyId,
                UIA_IsValuePatternAvailablePropertyId,
                UIA_IsTextPatternAvailablePropertyId,
                UIA_IsTextEditPatternAvailablePropertyId,
                UIA_ValueIsReadOnlyPropertyId,
            ] {
                cache.AddProperty(property)?;
            }
            Ok(Self { uia, walker, cache })
        }
    }

    fn snapshot(&self, point: POINT, deadline: Instant) -> Snapshot {
        let mut info = Snapshot::default();
        unsafe {
            // Read focus before walking the ancestors so both key facts are sampled early.
            let hit = self.uia.ElementFromPointBuildCache(point, &self.cache);
            if Instant::now() < deadline {
                match self.uia.GetFocusedElementBuildCache(&self.cache) {
                    Ok(focus) => info.focus = Some(read_node(&focus, &mut info.errors, deadline)),
                    Err(err) => info.errors.push(format!("focus: {}", crate::error(&err))),
                }
            }
            match hit {
                Ok(mut element) => {
                    for depth in 0..=6 {
                        if Instant::now() >= deadline {
                            break;
                        }
                        let node = read_node(&element, &mut info.errors, deadline);
                        info.chain.push(node);
                        if depth == 6 || Instant::now() >= deadline {
                            break;
                        }
                        match self
                            .walker
                            .GetParentElementBuildCache(&element, &self.cache)
                        {
                            Ok(parent) => element = parent,
                            Err(err) => {
                                // windows-rs maps the root's successful null result to an error
                                // with HRESULT 0 because IUIAutomationElement is non-null.
                                if err.code().is_err() {
                                    info.errors.push(format!(
                                        "ancestor {}: {}",
                                        depth + 1,
                                        crate::error(&err)
                                    ));
                                }
                                break;
                            }
                        }
                    }
                }
                Err(err) => info.errors.push(format!("hit: {}", crate::error(&err))),
            }
        }
        info
    }
}

fn variant_bool(mut value: VARIANT) -> Option<bool> {
    unsafe {
        let entry = &*value.Anonymous.Anonymous;
        let boolean = (entry.vt == VT_BOOL).then(|| entry.Anonymous.boolVal.0 != 0);
        let _ = VariantClear(&mut value);
        boolean
    }
}

#[allow(non_upper_case_globals)] // Use the Windows SDK's constants directly.
fn read_node(element: &IUIAutomationElement, errors: &mut Vec<String>, deadline: Instant) -> Node {
    unsafe {
        let read = |property, ignore_default| {
            element
                .GetCachedPropertyValueEx(property, ignore_default)
                .ok()
                .and_then(variant_bool)
        };
        let value = read(UIA_IsValuePatternAvailablePropertyId, false);
        let text = read(UIA_IsTextPatternAvailablePropertyId, false);
        let control = element.CachedControlType().ok();
        let text_read_only = if text == Some(true)
            && matches!(
                control,
                Some(UIA_EditControlTypeId | UIA_DocumentControlTypeId)
            )
            && Instant::now() < deadline
        {
            match element
                .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                .and_then(|pattern| pattern.DocumentRange())
                .and_then(|range| range.GetAttributeValue(UIA_IsReadOnlyAttributeId))
            {
                Ok(value) => variant_bool(value),
                Err(err) => {
                    errors.push(format!("TextPattern.IsReadOnly: {}", crate::error(&err)));
                    None
                }
            }
        } else {
            None
        };

        let mut name = element
            .CachedName()
            .map(|s| s.to_string())
            .unwrap_or_default();
        if let Some((index, _)) = name.char_indices().nth(80) {
            name.truncate(index);
            name.push('…');
        }
        Node {
            control,
            localized_control: element
                .CachedLocalizedControlType()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            class: element
                .CachedClassName()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            name,
            framework: element
                .CachedFrameworkId()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            focusable: element
                .CachedIsKeyboardFocusable()
                .ok()
                .map(|b| b.as_bool()),
            focused: element.CachedHasKeyboardFocus().ok().map(|b| b.as_bool()),
            password: element.CachedIsPassword().ok().map(|b| b.as_bool()),
            bounds: element.CachedBoundingRectangle().ok().map(|r| Bounds {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            }),
            pid: element
                .CachedProcessId()
                .ok()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0),
            value,
            text,
            text_edit: read(UIA_IsTextEditPatternAvailablePropertyId, false),
            value_read_only: if value == Some(true) {
                read(UIA_ValueIsReadOnlyPropertyId, true)
            } else {
                None
            },
            text_read_only,
        }
    }
}

struct Request {
    point: POINT,
    deadline: Instant,
    result: SyncSender<Snapshot>,
}

pub struct Worker(SyncSender<Request>);

impl Worker {
    pub fn start() -> std::result::Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<Request>(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let setup = Apartment::init(COINIT_MULTITHREADED)
                .and_then(|apartment| Automation::new().map(|automation| (apartment, automation)));
            let (_apartment, automation) = match setup {
                Ok(pair) => pair,
                Err(err) => {
                    let _ = ready_tx.send(Err(crate::error(&err)));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            while let Ok(request) = rx.recv() {
                if Instant::now() < request.deadline {
                    let snapshot = automation.snapshot(request.point, request.deadline);
                    let _ = request.result.send(snapshot);
                }
            }
            // Release the COM interfaces before uninitializing this apartment.
            drop(automation);
        });
        ready_rx
            .recv_timeout(LOOKUP_TIMEOUT)
            .map_err(|err| format!("UIA startup: {err}"))??;
        Ok(Self(tx))
    }

    pub fn lookup(&self, point: POINT) -> Snapshot {
        let deadline = Instant::now() + LOOKUP_TIMEOUT;
        let (tx, rx) = mpsc::sync_channel(1);
        let result = self.0.try_send(Request {
            point,
            deadline,
            result: tx,
        });
        let message = match result {
            Ok(()) => match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(snapshot) => return snapshot,
                Err(_) => "UI Automation lookup timed out after 250 ms",
            },
            Err(_) => "UI Automation worker busy or unavailable",
        };
        Snapshot {
            errors: vec![message.into()],
            ..Snapshot::default()
        }
    }
}

pub fn print_node(label: &str, node: &Node) {
    let bounds = node
        .bounds
        .map(|b| format!("[{}, {}, {}, {}]", b.left, b.top, b.right, b.bottom));
    println!(
        "{label} ControlType={:?} LocalizedControlType={:?} ClassName={:?} Name={:?} FrameworkId={:?} IsKeyboardFocusable={:?} HasKeyboardFocus={:?} BoundingRectangle={} ValuePattern={:?} TextPattern={:?} TextEditPattern={:?} ValuePattern.IsReadOnly={:?} TextPattern.IsReadOnly={:?} IsPassword={:?} PID={:?}",
        node.control.map(|id| id.0),
        node.localized_control,
        node.class,
        node.name,
        node.framework,
        node.focusable,
        node.focused,
        bounds.as_deref().unwrap_or("unknown"),
        node.value,
        node.text,
        node.text_edit,
        node.value_read_only,
        node.text_read_only,
        node.password,
        node.pid,
    );
}

pub fn int_variant(value: i32) -> VARIANT {
    use std::mem::ManuallyDrop;
    use windows::Win32::System::Variant::{VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}
