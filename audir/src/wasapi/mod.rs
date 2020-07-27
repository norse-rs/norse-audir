#![allow(non_upper_case_globals)]

pub mod com;
mod fence;

use self::fence::*;

pub use winapi::shared::winerror::HRESULT;
pub type WasapiResult<T> = (T, HRESULT);

use com::{Guid, WeakPtr};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::{ffi::OsString, mem, os::windows::ffi::OsStringExt, ptr, slice};
use winapi::shared::devpkey::*;
use winapi::shared::ksmedia;
use winapi::shared::minwindef::DWORD;
use winapi::shared::mmreg::*;
use winapi::shared::winerror;
use winapi::shared::wtypes::PROPERTYKEY;
use winapi::um::audioclient::*;
use winapi::um::audiosessiontypes::*;
use winapi::um::combaseapi::*;
use winapi::um::coml2api::STGM_READ;
use winapi::um::mmdeviceapi::*;
use winapi::um::objbase::COINIT_MULTITHREADED;
use winapi::um::propsys::*;
use winapi::um::winnt::*;

use winapi::Interface;

use crate::{
    api::{self, Result},
    handle::Handle,
};

#[derive(Debug)]
enum Event {
    Added(PhysicalDeviceId),
    Removed(PhysicalDeviceId),
    Changed {
        device: PhysicalDeviceId,
        state: u32,
    },
    Default {
        device: PhysicalDeviceId,
        flow: EDataFlow,
    },
}

unsafe fn string_from_wstr(os_str: *const WCHAR) -> String {
    let mut len = 0;
    while *os_str.offset(len) != 0 {
        len += 1;
    }
    let string: OsString = OsStringExt::from_wide(slice::from_raw_parts(os_str, len as _));
    string.into_string().unwrap()
}

#[repr(C)]
#[derive(com_impl::ComImpl)]
#[interfaces(IMMNotificationClient)]
pub struct NotificationClient {
    vtbl: com_impl::VTable<IMMNotificationClientVtbl>,
    refcount: com_impl::Refcount,
    tx: Sender<Event>,
}

#[com_impl::com_impl]
unsafe impl IMMNotificationClient for NotificationClient {
    unsafe fn on_device_state_changed(&self, pwstrDeviceId: LPCWSTR, state: DWORD) -> HRESULT {
        let _ = self.tx.send(Event::Changed {
            device: string_from_wstr(pwstrDeviceId),
            state,
        });
        winerror::S_OK
    }

    unsafe fn on_device_added(&self, pwstrDeviceId: LPCWSTR) -> HRESULT {
        let _ = self.tx.send(Event::Added(string_from_wstr(pwstrDeviceId)));
        winerror::S_OK
    }

    unsafe fn on_device_removed(&self, pwstrDeviceId: LPCWSTR) -> HRESULT {
        let _ = self
            .tx
            .send(Event::Removed(string_from_wstr(pwstrDeviceId)));
        winerror::S_OK
    }

    unsafe fn on_default_device_changed(
        &self,
        flow: EDataFlow,
        role: ERole,
        pwstrDefaultDeviceId: LPCWSTR,
    ) -> HRESULT {
        if role == eConsole {
            let _ = self.tx.send(Event::Default {
                device: string_from_wstr(pwstrDefaultDeviceId),
                flow,
            });
        }

        winerror::S_OK
    }

    unsafe fn on_property_value_changed(
        &self,
        pwstrDeviceId: LPCWSTR,
        key: PROPERTYKEY,
    ) -> HRESULT {
        winerror::S_OK
    }
}

fn map_frame_desc(frame_desc: &api::FrameDesc) -> Option<WAVEFORMATEXTENSIBLE> {
    let (format_tag, sub_format, bytes_per_sample) = match frame_desc.format {
        api::Format::F32 => (
            WAVE_FORMAT_EXTENSIBLE,
            ksmedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            4,
        ),
        api::Format::U32 => return None,
        _ => unimplemented!(),
    };

    let mut channel_mask = 0;
    {
        let channels = frame_desc.channels;
        if channels.contains(api::ChannelMask::FRONT_LEFT) {
            channel_mask |= SPEAKER_FRONT_LEFT;
        }
        if channels.contains(api::ChannelMask::FRONT_RIGHT) {
            channel_mask |= SPEAKER_FRONT_RIGHT;
        }
        if channels.contains(api::ChannelMask::FRONT_CENTER) {
            channel_mask |= SPEAKER_FRONT_CENTER;
        }
    }

    let num_channels = frame_desc.num_channels();
    let bits_per_sample = 8 * bytes_per_sample;

    let format = WAVEFORMATEX {
        wFormatTag: format_tag,
        nChannels: num_channels as _,
        nSamplesPerSec: frame_desc.sample_rate as _,
        nAvgBytesPerSec: (num_channels * frame_desc.sample_rate * bytes_per_sample) as _,
        nBlockAlign: (num_channels * bytes_per_sample) as _,
        wBitsPerSample: bits_per_sample as _,
        cbSize: (mem::size_of::<WAVEFORMATEXTENSIBLE>() - mem::size_of::<WAVEFORMATEX>()) as _,
    };

    Some(WAVEFORMATEXTENSIBLE {
        Format: format,
        Samples: bits_per_sample as _,
        dwChannelMask: channel_mask,
        SubFormat: sub_format,
    })
}

unsafe fn map_waveformat(format: *const WAVEFORMATEX) -> Result<api::FrameDesc> {
    let wave_format = &*format;
    match wave_format.wFormatTag {
        WAVE_FORMAT_EXTENSIBLE => {
            let wave_format_ex = &*(format as *const WAVEFORMATEXTENSIBLE);
            let subformat = Guid(wave_format_ex.SubFormat);
            let samples = wave_format_ex.Samples;
            let format =
                if subformat == Guid(ksmedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT) && samples == 32 {
                    api::Format::F32
                } else {
                    return Err(api::Error::Validation); // TODO
                };

            let mut channels = api::ChannelMask::empty();
            if wave_format_ex.dwChannelMask & SPEAKER_FRONT_LEFT != 0 {
                channels |= api::ChannelMask::FRONT_LEFT;
            }
            if wave_format_ex.dwChannelMask & SPEAKER_FRONT_RIGHT != 0 {
                channels |= api::ChannelMask::FRONT_RIGHT;
            }
            if wave_format_ex.dwChannelMask & SPEAKER_FRONT_CENTER != 0 {
                channels |= api::ChannelMask::FRONT_CENTER;
            }

            Ok(api::FrameDesc {
                format,
                channels,
                sample_rate: wave_format.nSamplesPerSec as _,
            })
        }
        _ => Err(api::Error::Validation), // TODO
    }
}

fn map_sharing_mode(sharing: api::SharingMode) -> AUDCLNT_SHAREMODE {
    match sharing {
        api::SharingMode::Exclusive => AUDCLNT_SHAREMODE_EXCLUSIVE,
        api::SharingMode::Concurrent => AUDCLNT_SHAREMODE_SHARED,
    }
}

type InstanceRaw = WeakPtr<IMMDeviceEnumerator>;
type PhysicalDeviceRaw = WeakPtr<IMMDevice>;
struct PhysicalDevice {
    device: PhysicalDeviceRaw,
    state: u32,
    audio_client: WeakPtr<IAudioClient>,
    streams: api::StreamFlags,
}

impl PhysicalDevice {
    // TODO: extension?
    // unsafe fn mix_format(&self) -> Result<api::FrameDesc> {
    //     let mut mix_format = ptr::null_mut();
    //     self.audio_client.GetMixFormat(&mut mix_format);
    //     map_waveformat(mix_format)
    // }
}

type PhysicalDeviceId = String;
type PhysialDeviceMap = HashMap<PhysicalDeviceId, Handle<PhysicalDevice>>;

pub struct Session(Option<audio_thread_priority::RtPriorityHandle>);

impl std::ops::Drop for Session {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            audio_thread_priority::demote_current_thread_from_real_time(handle).unwrap();
        }
    }
}

pub struct Instance {
    raw: InstanceRaw,
    physical_devices: PhysialDeviceMap,
    notifier: WeakPtr<NotificationClient>,
    event_rx: Receiver<Event>,
}

impl api::Instance for Instance {
    type Device = Device;
    type Stream = Stream;
    type Session = Session;

    unsafe fn properties() -> api::InstanceProperties {
        api::InstanceProperties {
            driver_id: api::DriverId::Wasapi,
            stream_mode: api::StreamMode::Polling,
            sharing: api::SharingModeFlags::CONCURRENT | api::SharingModeFlags::EXCLUSIVE,
        }
    }

    unsafe fn create(_: &str) -> Self {
        CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED);

        let mut instance = InstanceRaw::null();
        let _hr = CoCreateInstance(
            &CLSID_MMDeviceEnumerator,
            ptr::null_mut(),
            CLSCTX_ALL,
            &IMMDeviceEnumerator::uuidof(),
            instance.mut_void(),
        );

        let (tx, event_rx) = channel();
        let notification_client = NotificationClient::create_raw(tx);

        let mut physical_devices = HashMap::new();
        Self::enumerate_physical_devices_by_flow(&mut physical_devices, instance, eCapture);
        Self::enumerate_physical_devices_by_flow(&mut physical_devices, instance, eRender);

        Instance {
            raw: instance,
            physical_devices,
            notifier: WeakPtr::from_raw(notification_client),
            event_rx,
        }
    }

    unsafe fn enumerate_physical_devices(&self) -> Vec<api::PhysicalDevice> {
        self.physical_devices
            .values()
            .filter_map(|device| {
                if device.state & DEVICE_STATE_ACTIVE != 0 {
                    Some(device.raw())
                } else {
                    None
                }
            })
            .collect()
    }

    unsafe fn default_physical_input_device(&self) -> Option<api::PhysicalDevice> {
        let mut device = PhysicalDeviceRaw::null();
        let _hr = self
            .raw
            .GetDefaultAudioEndpoint(eCapture, eConsole, device.mut_void() as *mut _);
        if device.is_null() {
            None
        } else {
            let id = Self::get_physical_device_id(device);
            Some(self.physical_devices[&id].raw())
        }
    }

    unsafe fn default_physical_output_device(&self) -> Option<api::PhysicalDevice> {
        let mut device = PhysicalDeviceRaw::null();
        let _hr = self
            .raw
            .GetDefaultAudioEndpoint(eRender, eConsole, device.mut_void() as *mut _);
        if device.is_null() {
            None
        } else {
            let id = Self::get_physical_device_id(device);
            Some(self.physical_devices[&id].raw())
        }
    }

    unsafe fn physical_device_properties(
        &self,
        physical_device: api::PhysicalDevice,
    ) -> Result<api::PhysicalDeviceProperties> {
        type PropertyStore = WeakPtr<IPropertyStore>;

        let physical_device = Handle::<PhysicalDevice>::from_raw(physical_device);

        let mut store = PropertyStore::null();
        physical_device
            .device
            .OpenPropertyStore(STGM_READ, store.mut_void() as *mut _);

        let device_name = {
            let mut value = mem::MaybeUninit::uninit();
            store.GetValue(
                &DEVPKEY_Device_FriendlyName as *const _ as *const _,
                value.as_mut_ptr(),
            );
            let os_str = *value.assume_init().data.pwszVal();
            string_from_wstr(os_str)
        };

        Ok(api::PhysicalDeviceProperties {
            device_name,
            streams: physical_device.streams,
        })
    }

    unsafe fn create_device(
        &self,
        desc: api::DeviceDesc,
        channels: api::Channels,
        callback: api::StreamCallback<Stream>,
    ) -> Result<Device> {
        if !channels.input.is_empty() && !channels.output.is_empty() {
            // no duplex
            return Err(api::Error::Validation);
        }

        let physical_device = Handle::<PhysicalDevice>::from_raw(desc.physical_device);
        let sharing = map_sharing_mode(desc.sharing);

        let fence = Fence::create(false, false);

        let frame_desc = api::FrameDesc {
            format: desc.sample_desc.format,
            channels: if !channels.input.is_empty() { channels.input } else { channels.output },
            sample_rate: desc.sample_desc.sample_rate,
        };
        let mix_format = map_frame_desc(&frame_desc).unwrap(); // todo
        dbg!(physical_device.audio_client.Initialize(
            sharing,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            &mix_format as *const _ as _,
            ptr::null(),
        ));

        physical_device.audio_client.SetEventHandle(fence.0);

        let (stream, device_stream) = if !channels.input.is_empty() {
            let mut capture_client = WeakPtr::<IAudioCaptureClient>::null();
            physical_device.audio_client.GetService(
                &IAudioCaptureClient::uuidof(),
                capture_client.mut_void() as _,
            );
            let stream = unimplemented!();
            let device_stream = DeviceStream::Input {
                client: capture_client,
            };

            (stream, device_stream)
        } else {
            let mut render_client = WeakPtr::<IAudioRenderClient>::null();
            physical_device
                .audio_client
                .GetService(&IAudioRenderClient::uuidof(), render_client.mut_void() as _);
            let buffer_size = {
                let mut size = 0;
                physical_device.audio_client.GetBufferSize(&mut size);
                size
            };

            let mut mix_format = ptr::null_mut();
            physical_device.audio_client.GetMixFormat(&mut mix_format);

            let frame_desc = map_waveformat(mix_format).unwrap();

            let stream = Stream {
                properties: api::StreamProperties {
                    channels: frame_desc.channels,
                    sample_rate: frame_desc.sample_rate,
                    buffer_size: buffer_size as _,
                },
            };
            let device_stream = DeviceStream::Output {
                client: render_client,
                buffer_size,
            };

            (stream, device_stream)
        };

        Ok(Device {
            client: physical_device.audio_client,
            fence,
            device_stream,
            callback,
            stream,
        })
    }

    unsafe fn create_session(&self, sample_rate: usize) -> Result<Session> {
        let rt_handle = audio_thread_priority::promote_current_thread_to_real_time(0, sample_rate as _).unwrap();
        Ok(Session(Some(rt_handle)))
    }

    unsafe fn poll_events<F>(&self, _callback: F) -> Result<()>
    where
        F: FnMut(api::Event),
    {
        while let Ok(event) = self.event_rx.try_recv() {
            // TODO
            dbg!(event);
        }

        Ok(())
    }

    unsafe fn physical_device_supports_format(
        &self,
        physical_device: api::PhysicalDevice,
        sharing: api::SharingMode,
        frame_desc: api::FrameDesc,
    ) -> bool {
        let physical_device = Handle::<PhysicalDevice>::from_raw(physical_device);

        let wave_format = map_frame_desc(&frame_desc).unwrap(); // todo
        let sharing = map_sharing_mode(sharing);

        let mut closest_format = ptr::null_mut();
        let hr = dbg!(physical_device.audio_client.IsFormatSupported(
            sharing,
            &wave_format as *const _ as _,
            &mut closest_format
        ));

        hr == winerror::S_OK
    }
}

impl Instance {
    unsafe fn get_physical_device_id(device: PhysicalDeviceRaw) -> String {
        let mut str_id = ptr::null_mut();
        device.GetId(&mut str_id);
        let mut len = 0;
        while *str_id.offset(len) != 0 {
            len += 1;
        }
        let name: OsString = OsStringExt::from_wide(slice::from_raw_parts(str_id, len as _));
        name.into_string().unwrap()
    }

    unsafe fn enumerate_physical_devices_by_flow(
        physical_devices: &mut PhysialDeviceMap,
        instance: InstanceRaw,
        ty: EDataFlow,
    ) {
        type DeviceCollection = WeakPtr<IMMDeviceCollection>;

        let stream_flags = match ty {
            eCapture => api::StreamFlags::INPUT,
            eRender => api::StreamFlags::OUTPUT,
            _ => unreachable!(),
        };

        let collection = {
            let mut collection = DeviceCollection::null();
            let _hr = instance.EnumAudioEndpoints(
                ty,
                DEVICE_STATE_ACTIVE
                    | DEVICE_STATE_DISABLED
                    | DEVICE_STATE_NOTPRESENT
                    | DEVICE_STATE_UNPLUGGED,
                collection.mut_void() as *mut _,
            );
            collection
        };

        let num_items = {
            let mut num = 0;
            collection.GetCount(&mut num);
            num
        };

        for i in 0..num_items {
            let mut device = PhysicalDeviceRaw::null();
            collection.Item(i, device.mut_void() as *mut _);
            let id = Self::get_physical_device_id(device);
            let state = {
                let mut state = 0;
                device.GetState(&mut state);
                state
            };

            physical_devices
                .entry(id)
                .and_modify(|device| {
                    device.streams |= stream_flags;
                })
                .or_insert_with(|| {
                    let mut audio_client = WeakPtr::<IAudioClient>::null();

                    if state & DEVICE_STATE_ACTIVE != 0 {
                        device.Activate(
                            &IAudioClient::uuidof(),
                            CLSCTX_ALL,
                            ptr::null_mut(),
                            audio_client.mut_void() as *mut _,
                        );
                    }

                    Handle::new(PhysicalDevice {
                        device,
                        state,
                        audio_client,
                        streams: stream_flags,
                    })
                });
        }

        collection.Release();
    }
}

impl std::ops::Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            self.raw.Release();
            WeakPtr::from_raw(self.notifier.as_mut_ptr() as *mut IMMNotificationClient).Release();
            // TODO: drop audio clients
        }
    }
}

pub enum DeviceStream {
    Input {
        client: WeakPtr<IAudioCaptureClient>,
    },
    Output {
        client: WeakPtr<IAudioRenderClient>,
        buffer_size: u32,
    },
}

pub struct Stream {
    properties: api::StreamProperties,
}
pub struct Device {
    client: WeakPtr<IAudioClient>,
    fence: Fence,
    device_stream: DeviceStream,
    callback: api::StreamCallback<Stream>,
    stream: Stream,
}

impl std::ops::Drop for Device {
    fn drop(&mut self) {
        unsafe {
            self.client.Release();
            self.fence.destory();
        }
    }
}

impl Device {
    unsafe fn acquire_buffers(&mut self, timeout_ms: u32) -> Result<api::StreamBuffers> {
        self.fence.wait(timeout_ms);

        match self.device_stream {
            DeviceStream::Input { client } => {
                let mut len = 0;
                client.GetNextPacketSize(&mut len);

                let mut data = ptr::null_mut();
                let mut num_frames = 0;
                let mut flags = 0;

                client.GetBuffer(
                    &mut data,
                    &mut num_frames,
                    &mut flags,
                    ptr::null_mut(),
                    ptr::null_mut(),
                );

                if flags != 0 {
                    dbg!(flags);
                }

                Ok(api::StreamBuffers {
                    frames: num_frames as _,
                    input: data as _,
                    output: ptr::null_mut(),
                })
            }
            DeviceStream::Output {
                client,
                buffer_size,
            } => {
                let mut data = ptr::null_mut();
                let mut padding = 0;

                self.client.GetCurrentPadding(&mut padding);

                let len = buffer_size - padding;
                client.GetBuffer(len, &mut data);
                Ok(api::StreamBuffers {
                    frames: len as _,
                    input: ptr::null(),
                    output: data as _,
                })
            }
        }
    }

    unsafe fn release_buffers(&mut self, num_frames: api::Frames) -> Result<()> {
        match self.device_stream {
            DeviceStream::Input { client } => {
                client.ReleaseBuffer(num_frames as _);
            }
            DeviceStream::Output { client, .. } => {
                client.ReleaseBuffer(num_frames as _, 0);
            }
        }
        Ok(())
    }
}

impl api::Device for Device {
    unsafe fn start(&self) {
        self.client.Start();
    }

    unsafe fn stop(&self) {
        self.client.Stop();
    }

    unsafe fn submit_buffers(&mut self, timeout_ms: u32) -> Result<()> {
        let buffers = self.acquire_buffers(timeout_ms)?;
        (self.callback)(&self.stream, buffers);
        self.release_buffers(buffers.frames)
    }
}

impl api::Stream for Stream {
    unsafe fn properties(&self) -> api::StreamProperties {
        self.properties.clone()
    }
}
