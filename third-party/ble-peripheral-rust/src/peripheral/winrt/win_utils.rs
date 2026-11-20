use std::{collections::HashMap, str::FromStr};

use uuid::Uuid;
use windows::{
    core::{Error, GUID},
    Devices::Bluetooth::GenericAttributeProfile::{
        GattLocalCharacteristic, GattServiceProvider, GattSession, GattSubscribedClient,
    },
    Foundation::EventRegistrationToken,
    Storage::Streams::{DataReader, DataWriter, IBuffer, InMemoryRandomAccessStream},
};

pub struct GattCharacteristicObject {
    pub obj: GattLocalCharacteristic,
    pub subscribed_clients: Vec<GattSubscribedClient>,
    pub subscribed_clients_token: EventRegistrationToken,
    pub read_requested_token: EventRegistrationToken,
    pub write_requested_token: EventRegistrationToken,
}

pub struct GattServiceProviderObject {
    pub obj: GattServiceProvider,
    pub advertisement_status_changed_token: EventRegistrationToken,
    pub characteristics: HashMap<Uuid, GattCharacteristicObject>,
}

pub(crate) fn to_guid(uuid: &Uuid) -> GUID {
    let (g1, g2, g3, g4) = uuid.as_fields();
    GUID::from_values(g1, g2, g3, g4.clone())
}

// BlueLink 补丁：以下三个工具函数会被 WinRT 回调路径调用，原版全部 unwrap。
// 它们运行在 FFI 回调线程上时 panic 即进程 abort，故一律改为失败兜底
// （nil UUID / 空数据 / 空 buffer），只记 log 不 panic。

pub(crate) fn to_uuid(uuid: &GUID) -> Uuid {
    let guid_s = format!("{:?}", uuid);
    Uuid::from_str(&guid_s).unwrap_or_else(|e| {
        log::error!("Failed to parse GUID {guid_s}: {e}");
        Uuid::nil()
    })
}

pub(crate) fn buffer_to_vec(buffer: &IBuffer) -> Vec<u8> {
    fn read_all(buffer: &IBuffer) -> Option<Vec<u8>> {
        let reader = DataReader::FromBuffer(buffer).ok()?;
        let len = reader.UnconsumedBufferLength().ok()? as usize;
        let mut data = vec![0u8; len];
        reader.ReadBytes(&mut data).ok()?;
        Some(data)
    }
    read_all(buffer).unwrap_or_else(|| {
        log::error!("buffer_to_vec failed, treating as empty");
        Vec::new()
    })
}

pub(crate) fn vec_to_buffer(vector: Vec<u8>) -> IBuffer {
    fn build(vector: &[u8]) -> Option<IBuffer> {
        let stream = InMemoryRandomAccessStream::new().ok()?;
        let data_writer = DataWriter::CreateDataWriter(&stream).ok()?;
        data_writer.WriteBytes(vector).ok()?;
        data_writer.DetachBuffer().ok()
    }
    if let Some(buf) = build(&vector) {
        return buf;
    }
    log::error!("vec_to_buffer failed, returning empty buffer");
    if let Some(buf) = build(&[]) {
        return buf;
    }
    // 兜底链末端：0 容量 Buffer 是 WinRT 运行时保证可构造的对象。
    // 再往后的 expect 只存在于「连空 buffer 都建不出来」的不可恢复状态，
    // 正常失败路径（单次调用失败）全部在上面被优雅处理。
    use windows::core::Interface;
    match windows::Storage::Streams::Buffer::Create(0) {
        Ok(b) => b
            .cast()
            .unwrap_or_else(|_| build(&[]).expect("vec_to_buffer: cannot build any IBuffer")),
        Err(_) => build(&[]).expect("vec_to_buffer: cannot build any IBuffer"),
    }
}

pub(crate) fn device_id_from_session(session: GattSession) -> String {
    if let Ok(id) = get_complete_device_id(session) {
        if let Some(id) = id.split("-").last() {
            return id.to_string();
        }
        return id;
    }
    return "".to_string();
}

fn get_complete_device_id(session: GattSession) -> Result<String, Error> {
    if let Ok(id) = session.DeviceId()?.Id()?.to_os_string().into_string() {
        return Ok(id);
    }
    return Ok("".to_string());
}
