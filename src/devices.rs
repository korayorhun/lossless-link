//! Windows audio endpoints: listing outputs, the default output, endpoint volume, thread priority.

use anyhow::Result;
use windows::core::{interface, HRESULT, HSTRING, IUnknown, IUnknown_Vtbl, GUID, PCWSTR, PROPVARIANT};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, ERole, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE, PKEY_AudioEndpoint_Disable_SysFx, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;

pub const ALL_ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

/// Joins the multithreaded COM apartment; every thread that touches audio endpoints calls this first.
pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Lets Windows' multimedia scheduler (MMCSS) treat the calling thread as pro audio, so heavy CPU load
/// elsewhere can't starve it. The Python prototype turned choppy under load without this.
pub fn pro_audio_thread() {
    let mut index = 0u32;
    if unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut index) }.is_err() {
        log::warn!("MMCSS is not available for this thread");
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

pub fn device(id: &str) -> Result<IMMDevice> {
    Ok(unsafe { enumerator()?.GetDevice(&HSTRING::from(id))? })
}

fn id_of(device: &IMMDevice) -> Result<String> {
    unsafe {
        let p = device.GetId()?;
        let id = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(id?)
    }
}

fn name_of(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let value = store.GetValue(&PKEY_Device_FriendlyName)?;
        let p = PropVariantToStringAlloc(&value)?;
        let name = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(name?)
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub id: String,
    pub name: String,
}

/// Playback devices that are active right now.
pub fn outputs() -> Result<Vec<Output>> {
    unsafe {
        let collection = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut list = Vec::new();
        for i in 0..collection.GetCount()? {
            let device = collection.Item(i)?;
            list.push(Output { id: id_of(&device)?, name: name_of(&device).unwrap_or_default() });
        }
        Ok(list)
    }
}

pub fn default_output(role: ERole) -> Result<String> {
    unsafe { id_of(&enumerator()?.GetDefaultAudioEndpoint(eRender, role)?) }
}

#[allow(non_snake_case)]
mod policy_config {
    use super::*;

    /// Undocumented but long-stable interface that Windows' own Sound settings use to change the default
    /// device, device formats and endpoint properties; unused entries just keep the vtable order.
    #[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    pub unsafe trait IPolicyConfig: IUnknown {
        fn GetMixFormat(&self) -> HRESULT;
        fn GetDeviceFormat(&self) -> HRESULT;
        fn ResetDeviceFormat(&self) -> HRESULT;
        fn SetDeviceFormat(&self, device_id: PCWSTR, endpoint: *const WAVEFORMATEX, mix: *const WAVEFORMATEX) -> HRESULT;
        fn GetProcessingPeriod(&self) -> HRESULT;
        fn SetProcessingPeriod(&self) -> HRESULT;
        fn GetShareMode(&self) -> HRESULT;
        fn SetShareMode(&self) -> HRESULT;
        fn GetPropertyValue(&self) -> HRESULT;
        fn SetPropertyValue(&self, device_id: PCWSTR, fx_store: BOOL, key: *const PROPERTYKEY, value: *const PROPVARIANT) -> HRESULT;
        fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
        fn SetEndpointVisibility(&self) -> HRESULT;
    }

    const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

    fn policy() -> Result<IPolicyConfig> {
        Ok(unsafe { CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)? })
    }

    pub fn set_default_output(id: &str, roles: &[ERole]) -> Result<()> {
        let policy = policy()?;
        let id = HSTRING::from(id);
        for &role in roles {
            unsafe { policy.SetDefaultEndpoint(PCWSTR(id.as_ptr()), role).ok()? };
        }
        Ok(())
    }

    /// Turns off the driver's effects ("audio enhancements") on an output.
    pub fn disable_enhancements(id: &str) -> Result<()> {
        let id = HSTRING::from(id);
        let off = PROPVARIANT::from(1u32);
        unsafe {
            policy()?.SetPropertyValue(PCWSTR(id.as_ptr()), BOOL(0), &PKEY_AudioEndpoint_Disable_SysFx, &off).ok()?;
        }
        Ok(())
    }

    fn stereo_44k1(bits: u16, float: bool) -> WAVEFORMATEXTENSIBLE {
        let block = 2 * bits / 8;
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE, // WAVE_FORMAT_EXTENSIBLE
                nChannels: 2,
                nSamplesPerSec: 44_100,
                nAvgBytesPerSec: 44_100 * u32::from(block),
                nBlockAlign: block,
                wBitsPerSample: bits,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: bits },
            dwChannelMask: 3, // front left + front right
            SubFormat: GUID::from_u128(if float { 0x00000003_0000_0010_8000_00aa00389b71 } else { 0x00000001_0000_0010_8000_00aa00389b71 }),
        }
    }

    /// 16-bit/44.1 kHz on the wire; Windows then mixes in 32-bit float at 44.1 kHz.
    pub fn set_cd_format(id: &str) -> Result<()> {
        let (endpoint, mix) = (stereo_44k1(16, false), stereo_44k1(32, true));
        let id = HSTRING::from(id);
        unsafe {
            policy()?
                .SetDeviceFormat(PCWSTR(id.as_ptr()), (&endpoint as *const WAVEFORMATEXTENSIBLE).cast(), (&mix as *const WAVEFORMATEXTENSIBLE).cast())
                .ok()?;
        }
        Ok(())
    }
}
pub use policy_config::set_default_output;

fn enhancements_disabled(id: &str) -> Result<bool> {
    unsafe {
        let value = device(id)?.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_AudioEndpoint_Disable_SysFx)?;
        Ok(u32::try_from(&value).unwrap_or(0) == 1)
    }
}

fn mix_rate(id: &str) -> Result<u32> {
    unsafe {
        let client: IAudioClient = device(id)?.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let rate = (*format).nSamplesPerSec;
        CoTaskMemFree(Some(format as *const _));
        Ok(rate)
    }
}

/// Nothing is connected to the capture output, so its driver effects (Realtek, DTS and the like) only alter the
/// sound before it is captured, and a rate other than 44.1 kHz only forces resampling. Turn the effects off and
/// run the output at 44.1 kHz, so CD-rate sources reach the speaker untouched.
pub fn prepare_capture_output(id: &str) {
    if !enhancements_disabled(id).unwrap_or(false) {
        match policy_config::disable_enhancements(id) {
            Ok(()) => log::info!("turned off audio enhancements on the capture output"),
            Err(e) => log::warn!("couldn't turn off audio enhancements on the capture output: {e:#}"),
        }
    }
    match mix_rate(id) {
        Ok(44_100) => {}
        Ok(rate) => match policy_config::set_cd_format(id) {
            Ok(()) => log::info!("capture output changed from {rate} Hz to 44.1 kHz"),
            Err(e) => log::warn!("couldn't set the capture output to 44.1 kHz: {e:#}"),
        },
        Err(e) => log::warn!("reading the capture output format: {e:#}"),
    }
}

/// Volume slider and mute of one output. COM objects stay on the thread that made them.
pub struct Volume(IAudioEndpointVolume);

impl Volume {
    pub fn of(id: &str) -> Result<Self> {
        Ok(Self(unsafe { device(id)?.Activate(CLSCTX_ALL, None)? }))
    }

    /// (slider 0..1, gain in dB, muted)
    pub fn read(&self) -> Result<(f32, f32, bool)> {
        unsafe {
            Ok((
                self.0.GetMasterVolumeLevelScalar()?,
                self.0.GetMasterVolumeLevel()?,
                self.0.GetMute()?.as_bool(),
            ))
        }
    }

    pub fn set_slider(&self, value: f32) -> Result<()> {
        unsafe { Ok(self.0.SetMasterVolumeLevelScalar(value.clamp(0.0, 1.0), std::ptr::null())?) }
    }
}
