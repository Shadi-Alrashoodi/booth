// The encoder settings Media Foundation reaches through ICodecAPI. Every
// encoder takes a different subset, so each setting is asked for with
// IsSupported first and what happened is kept for the log.

use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::Media::MediaFoundation::{ICodecAPI, IMFTransform};
use windows::Win32::System::Com::{IErrorInfo, SetErrorInfo};
use windows::Win32::System::Ole::CreateErrorInfo;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, w};

struct Taken {
    what: String,
    key: GUID,
    value: VARIANT,
}

pub(crate) struct CodecApi {
    api: Option<ICodecAPI>,
    taken: Vec<Taken>,
    refused: Vec<String>,
    notes: Vec<String>,
}

impl CodecApi {
    pub(crate) fn of(transform: &IMFTransform) -> CodecApi {
        CodecApi {
            api: transform.cast().ok(),
            taken: Vec::new(),
            refused: Vec::new(),
            notes: Vec::new(),
        }
    }

    // S_FALSE means not supported as well, and IsSupported's binding turns
    // it into Ok, so the call goes through the vtable.
    pub(crate) fn supports(&self, key: &GUID) -> bool {
        // SAFETY: a query on a live interface with a valid GUID pointer.
        self.api.as_ref().is_some_and(|api| unsafe {
            (Interface::vtable(api).IsSupported)(api.as_raw(), key) == S_OK
        })
    }

    /// Sets a value now, for the settings that change while encoding.
    pub(crate) fn set(&self, key: &GUID, value: &VARIANT) -> windows::core::Result<()> {
        let Some(api) = &self.api else {
            return Err(windows::Win32::Foundation::E_NOINTERFACE.into());
        };
        // SAFETY: a live interface, a valid GUID and VARIANT for the call.
        let result = unsafe { (Interface::vtable(api).SetValue)(api.as_raw(), key, value) };
        // S_FALSE says the property is read-only, so nothing was set, and
        // SetValue's binding turns it into Ok as well.
        if result == S_FALSE {
            return Err(read_only());
        }
        result.ok()
    }

    pub(crate) fn value(&self, key: &GUID) -> Option<u32> {
        let api = self.api.as_ref()?;
        // SAFETY: a live interface and a valid GUID; the VARIANT is owned
        // and cleared when dropped.
        let value = unsafe { api.GetValue(key) }.ok()?;
        u32::try_from(&value).ok()
    }

    /// Sets a value at open and notes whether the encoder took it. `what` is
    /// how the log names the setting.
    pub(crate) fn setting(&mut self, what: String, key: &GUID, value: VARIANT) -> bool {
        if self.api.is_none() {
            self.refused.push(format!("{what} (no ICodecAPI)"));
            return false;
        }
        if !self.supports(key) {
            self.refused.push(format!("{what} (not supported)"));
            return false;
        }
        match self.set(key, &value) {
            Ok(()) => {
                self.taken.push(Taken {
                    what,
                    key: *key,
                    value,
                });
                true
            }
            Err(e) => {
                self.refused.push(format!("{what} ({})", e.message()));
                false
            }
        }
    }

    /// A DWORD setting that some encoders declare as a VARIANT_BOOL instead.
    pub(crate) fn switch_on(&mut self, what: &str, key: &GUID) -> bool {
        if self.supports(key) && self.set(key, &VARIANT::from(1u32)).is_ok() {
            self.taken.push(Taken {
                what: what.to_string(),
                key: *key,
                value: VARIANT::from(1u32),
            });
            return true;
        }
        self.setting(what.to_string(), key, VARIANT::from(true))
    }

    /// Setting the output type puts some values back to the encoder's own
    /// defaults (Windows' software encoder does this to quality against
    /// speed), so everything taken is read back afterwards and set again
    /// where it moved.
    pub(crate) fn reapply(&mut self) {
        let mut kept = Vec::new();
        for taken in std::mem::take(&mut self.taken) {
            let wanted = u32::try_from(&taken.value).ok();
            if self.value(&taken.key) == wanted || self.set(&taken.key, &taken.value).is_ok() {
                kept.push(taken);
            } else {
                self.refused
                    .push(format!("{} (undone by the output type)", taken.what));
            }
        }
        self.taken = kept;
    }

    pub(crate) fn note(&mut self, text: String) {
        self.notes.push(text);
    }

    pub(crate) fn report(&self) -> String {
        let list = |items: Vec<&str>| {
            if items.is_empty() {
                "none".to_string()
            } else {
                items.join(", ")
            }
        };
        let mut report = format!(
            "settings taken: {}; refused: {}",
            list(self.taken.iter().map(|t| t.what.as_str()).collect()),
            list(self.refused.iter().map(String::as_str).collect())
        );
        for note in &self.notes {
            report.push_str("; ");
            report.push_str(note);
        }
        report
    }
}

// Windows' own text for S_FALSE is "Incorrect function", so the error is
// given the one SetValue means by it.
fn read_only() -> windows::core::Error {
    // SAFETY: a new error object, put on this thread for the conversion
    // below, which takes it straight back off.
    unsafe {
        if let Ok(info) = CreateErrorInfo() {
            let _ = info.SetDescription(w!("the property is read-only"));
            if let Ok(info) = info.cast::<IErrorInfo>() {
                let _ = SetErrorInfo(0, &info);
            }
        }
    }
    S_FALSE.into()
}
