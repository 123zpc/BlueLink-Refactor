//! simpleble-probe：CI 构建探针。
//!
//! 目的只有一个：证明 simplersble（SimpleBLE 的 Rust 绑定）在目标平台上
//! 不只是能链接，而是中心端与外设端两类 API 都能通过编译。
//! CI 只编译、不运行（runner 无蓝牙硬件）；本地手动运行时可用
//! `central` / `peripheral` 两个子命令分别走两条路径。

use std::time::Duration;

use simplersble::{Adapter, CharacteristicCapability};

// 探针用的固定 UUID（仅编译期占位，与产品协议无关）。
const SERVICE_UUID: &str = "b1000001-1234-4000-8000-000000000001";
const CHARACTERISTIC_UUID: &str = "b1000001-1234-4000-8000-000000000002";

/// 中心端路径：枚举适配器 → 扫描 → 读扫描结果与已配对设备。
fn probe_central() -> Result<(), simplersble::Error> {
    let enabled = Adapter::bluetooth_enabled()?;
    println!("bluetooth_enabled={enabled}");

    let adapters = Adapter::get_adapters()?;
    println!("adapters={}", adapters.len());

    if let Some(adapter) = adapters.into_iter().next() {
        println!(
            "adapter identifier={:?} address={:?} powered={:?}",
            adapter.identifier(),
            adapter.address(),
            adapter.is_powered()
        );

        adapter.scan_for(Duration::from_millis(500))?;
        println!("scan_is_active={:?}", adapter.scan_is_active());

        let results = adapter.scan_get_results()?;
        for peripheral in &results {
            println!(
                "found identifier={:?} address={:?} rssi={:?} connectable={:?}",
                peripheral.identifier(),
                peripheral.address(),
                peripheral.rssi(),
                peripheral.is_connectable()
            );
        }

        let paired = adapter.get_paired_peripherals()?;
        println!("paired_peripherals={}", paired.len());
    }
    Ok(())
}

/// 外设端路径：在适配器上建 LocalPeripheral → 加服务/特征 →
/// 设置广播内容 → 开始/停止广播。
fn probe_peripheral() -> Result<(), simplersble::Error> {
    let adapters = Adapter::get_adapters()?;
    let Some(adapter) = adapters.into_iter().next() else {
        println!("no adapter, skip peripheral probe");
        return Ok(());
    };

    let local = adapter.create_local_peripheral()?;

    let service = local.add_service(SERVICE_UUID)?;
    println!("service uuid={:?}", service.uuid());

    let characteristic = service.add_characteristic(
        CHARACTERISTIC_UUID,
        &[
            CharacteristicCapability::Read,
            CharacteristicCapability::WriteRequest,
            CharacteristicCapability::Notify,
        ],
    )?;
    characteristic.set_value(&[0x42, 0x4C])?;
    println!(
        "characteristic uuid={:?} capabilities={:?} value={:?}",
        characteristic.uuid(),
        characteristic.capabilities(),
        characteristic.value()
    );
    println!("service characteristics={}", service.characteristics().len());

    local.add_advertised_service(SERVICE_UUID)?;
    local.set_advertised_manufacturer_data(0xFFFF, &[0x42, 0x4C])?;
    local.set_connectable(true)?;

    local.start()?;
    println!(
        "started={:?} advertising={:?}",
        local.is_started(),
        local.is_advertising()
    );
    local.stop()?;
    Ok(())
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("central") => {
            if let Err(e) = probe_central() {
                eprintln!("central probe error: {e:?}");
                std::process::exit(1);
            }
        }
        Some("peripheral") => {
            if let Err(e) = probe_peripheral() {
                eprintln!("peripheral probe error: {e:?}");
                std::process::exit(1);
            }
        }
        _ => println!("simpleble-probe: 构建探针，CI 只编译不运行。用法: simpleble-probe [central|peripheral]"),
    }
}
